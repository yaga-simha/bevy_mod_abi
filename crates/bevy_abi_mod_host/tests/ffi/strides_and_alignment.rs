//! Heterogeneous alignment, `BamColumnInfo` layout introspection, ZSTs, dynamic component layouts,
//! and allocator isolation (`ABI 0.2.0`).

mod heterogeneous_alignment_and_isolation {
    use crate::support::{MetadataTag, MetadataTagVTable, Position};
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, host_iter_column_info, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_mark_changed_range, host_iter_next_chunk,
        host_sys_query,
    };
    use bevy_abi_mod_sys::{BamColumnInfo, BamIter, BamSystemCtx, BamTermDesc, FfiResult, StrView};
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C, align(1))]
    #[mod_component(name = "align_suite::AlignByte")]
    pub struct AlignByte {
        pub val: u8,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C, align(4))]
    #[mod_component(name = "align_suite::AlignWord")]
    pub struct AlignWord {
        pub val: u32,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C, align(8))]
    #[mod_component(name = "align_suite::AlignDWord")]
    pub struct AlignDWord {
        pub val: u64,
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C, align(16))]
    #[mod_component(name = "align_suite::AlignSimd")]
    pub struct AlignSimd {
        pub val: [f32; 4],
    }

    #[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
    #[repr(C, align(64))]
    #[mod_component(name = "align_suite::AlignCacheLine")]
    pub struct AlignCacheLine {
        pub val: [u64; 8],
    }

    unsafe extern "C" fn alignment_verifier_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let verified_counter = unsafe { &*(user_data as *const AtomicUsize) };
        let alignments = [1usize, 4, 8, 16, 64];
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            for (col_idx, &align) in alignments.iter().enumerate() {
                let ptr = unsafe { host_iter_column_readonly(iter, col_idx) };
                let mut info = MaybeUninit::<BamColumnInfo>::uninit();
                assert_eq!(
                    unsafe { host_iter_column_info(iter, col_idx, info.as_mut_ptr()) },
                    FfiResult::OK
                );
                let info = unsafe { info.assume_init() };
                let stride = info.size;

                assert!(!ptr.is_null(), "Column {col_idx} pointer must not be null");
                assert_eq!(info.align, align);
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
            verified_counter.fetch_add(count, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    /// Test 1: Alignment verification across 1, 4, 8, 16, 64 bytes.
    #[test]
    fn test_heterogeneous_memory_alignment() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let h_byte = app
            .register_mod_component::<AlignByte>(ComponentOptions::new())
            .unwrap();
        let h_word = app
            .register_mod_component::<AlignWord>(ComponentOptions::new())
            .unwrap();
        let h_dword = app
            .register_mod_component::<AlignDWord>(ComponentOptions::new())
            .unwrap();
        let h_simd = app
            .register_mod_component::<AlignSimd>(ComponentOptions::new())
            .unwrap();
        let h_cache = app
            .register_mod_component::<AlignCacheLine>(ComponentOptions::new())
            .unwrap();

        const COUNT: usize = 128;
        for i in 0..COUNT {
            app.world_mut().spawn((
                AlignByte {
                    val: (i % 255) as u8,
                },
                AlignWord { val: i as u32 },
                AlignDWord {
                    val: 0xDEAD_BEEF_CAFE_0000 + i as u64,
                },
                AlignSimd {
                    val: [1.0, 2.0, 3.0, i as f32],
                },
                AlignCacheLine { val: [i as u64; 8] },
            ));
        }

        let verified = Box::new(AtomicUsize::new(0));
        let registry = app.world().resource::<ModRegistry>().clone();
        let query_specs = vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::read(h_byte),
                BamTermDesc::read(h_word),
                BamTermDesc::read(h_dword),
                BamTermDesc::read(h_simd),
                BamTermDesc::read(h_cache),
            ],
        }];

        let sys = GuestSystem::new(
            "alignment_verifier",
            query_specs,
            alignment_verifier_callback,
            (&*verified as *const AtomicUsize) as *mut c_void,
            registry,
        );

        app.add_systems(Update, sys);
        app.update();

        assert_eq!(
            verified.load(Ordering::SeqCst),
            COUNT,
            "All 128 entities must have their 1B, 4B, 8B, 16B, and 64B alignments verified"
        );
    }

    unsafe extern "C" fn bounds_checker_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let runs = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        unsafe {
            let mut c1 = 99usize;
            assert!(!host_iter_next_chunk(iter, &mut c1).get());
            assert_eq!(c1, 0, "Empty table must yield 0 entities");

            let mut c2 = 99usize;
            assert!(!host_iter_next_chunk(iter, &mut c2).get());
            assert_eq!(c2, 0, "Exhausted iterator must return 0");

            assert!(host_iter_column_readonly(iter, 0).is_null());
            assert!(host_iter_column_mutable(iter, 0).is_null());

            let err = host_iter_mark_changed_range(iter, 0, 999, 1);
            assert_eq!(err, FfiResult::OUT_OF_RANGE);
        }

        runs.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    /// Test 2: Bounds checking on empty tables (0 entities) and iterator exhaustion.
    #[test]
    fn test_chunk_iterator_bounds_and_empty_tables() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        let runs = Box::new(AtomicUsize::new(0));
        let registry = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "bounds_checker",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(pos_h)],
            }],
            bounds_checker_callback,
            (&*runs as *const AtomicUsize) as *mut c_void,
            registry,
        );

        app.add_systems(Update, sys);

        // 1. Completely empty world (0 entities)
        app.update();
        assert_eq!(runs.load(Ordering::SeqCst), 1);

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
        assert_eq!(runs.load(Ordering::SeqCst), 2);
    }

    /// Test 3: Memory allocator boundary isolation (including self-aliasing `StrView` overlap defense).
    #[test]
    fn test_memory_allocator_boundary_isolation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let tag_h = app
            .register_mod_component::<MetadataTag>(ComponentOptions::new())
            .unwrap();

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

        let tag_vt_ptr = app
            .world()
            .resource::<ModRegistry>()
            .get_by_handle(tag_h)
            .expect("MetadataTag")
            .vtable_ptr
            .as_ptr() as *const MetadataTagVTable;
        let tag_vt = unsafe { &*tag_vt_ptr };

        // Mutate across boundary using guest-provided buffer, then exercise self-aliasing slice copy
        for (i, &e) in entities.iter().enumerate() {
            let mut tag_mut = app.world_mut().get_mut::<MetadataTag>(e).unwrap();
            let ptr = (&mut *tag_mut as *mut MetadataTag).cast::<c_void>();

            let guest_msg = format!("guest_payload_{i}");
            unsafe {
                assert_eq!(
                    (tag_vt.set_label.unwrap())(ptr, StrView::from_str(&guest_msg)),
                    FfiResult::OK
                );
                // Passing a sub-slice of the component's own buffer back into `set_label` must not UB!
                let self_view = (tag_vt.get_label.unwrap())(ptr.cast_const());
                assert_eq!((tag_vt.set_label.unwrap())(ptr, self_view), FfiResult::OK);
            }
        }

        for (i, &e) in entities.iter().enumerate() {
            let tag = app.world().get::<MetadataTag>(e).unwrap();
            assert_eq!(tag.label, format!("guest_payload_{i}"));
        }

        for e in entities {
            app.world_mut().despawn(e);
        }

        app.update();
    }
}

mod dynamic_component_lifecycle {
    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, LoadContext,
        host_cmd_despawn, host_cmd_insert, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_mutable_untracked, host_iter_mark_changed_range, host_iter_next_chunk,
        host_register_component, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc, ComponentUuid, FfiEntity,
        FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[test]
    fn test_dynamic_component_registration() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let is_active = Arc::new(AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(registry, is_active, &mut world as *mut World, None);

        let name = c"dyn_life::CustomDynamicHealth";
        let uuid = ComponentUuid::from_parts("dyn_life::CustomDynamicHealth", 8, 4, 0);
        let desc = ComponentDesc::new(name.as_ptr(), uuid, 8, 4);

        let mut handle = BamComponent::INVALID;
        let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) };
        assert_eq!(res, FfiResult::OK);
        assert_eq!(handle, BamComponent::from_raw(1));

        // Idempotent re-registration with identical layout must succeed and return same handle
        let mut handle_again = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle_again) },
            FfiResult::OK
        );
        assert_eq!(handle_again, handle);

        // Conflicting re-registration with mismatched size must fail with `TYPE_MISMATCH`
        let conflicting_desc = ComponentDesc::new(name.as_ptr(), uuid, 16, 4);
        assert_eq!(
            unsafe {
                host_register_component(load_ctx.as_ffi_ptr(), &conflicting_desc, &mut handle_again)
            },
            FfiResult::TYPE_MISMATCH
        );
        load_ctx.finish_init();

        let world_reg = world.resource::<ModRegistry>();
        let comp_meta = world_reg
            .get_by_name("dyn_life::CustomDynamicHealth")
            .expect("Component must be registered in ModRegistry");
        assert_eq!(comp_meta.handle, handle);
        assert_eq!(comp_meta.size, 8);
        assert_eq!(comp_meta.align, 4);
        assert_eq!(comp_meta.uuid, uuid);
        assert!(comp_meta.is_dynamic);
        assert!(comp_meta.insert_raw_fn.is_none());

        let comp_by_uuid = world_reg
            .get_by_uuid(&uuid)
            .expect("Component must be resolvable by UUID");
        assert_eq!(comp_by_uuid.component_id, comp_meta.component_id);
        assert!(
            world
                .components()
                .get_info(comp_meta.component_id)
                .is_some()
        );
    }

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
        world.insert_resource(registry.clone());

        let name = c"dyn_life::CustomVelocity";
        let uuid = ComponentUuid::from_parts(
            "dyn_life::CustomVelocity",
            core::mem::size_of::<CustomVelocity>(),
            core::mem::align_of::<CustomVelocity>(),
            0,
        );
        let desc = ComponentDesc::new(
            name.as_ptr(),
            uuid,
            core::mem::size_of::<CustomVelocity>(),
            core::mem::align_of::<CustomVelocity>(),
        );

        let mut handle = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let comp_id = snap.get_by_handle(handle).unwrap().component_id;

        let mut storage = HostCommandsStorage::new();
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
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            for vel in &vel_data {
                let mut ffi_entity = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_entity) },
                    FfiResult::OK
                );
                assert!(!ffi_entity.is_null());

                let insert_res = unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_entity,
                        handle,
                        (vel as *const CustomVelocity).cast::<u8>(),
                        core::mem::size_of::<CustomVelocity>(),
                    )
                };
                assert_eq!(insert_res, FfiResult::OK);
                spawned_entities.push(ffi_entity);
            }
        }

        storage.apply_to_world(&mut world, &snap);

        for (i, &ffi_entity) in spawned_entities.iter().enumerate() {
            let bevy_entity = Entity::from_bits(ffi_entity.0);
            let entity_ref = world.entity(bevy_entity);
            let comp_ptr = entity_ref
                .get_by_id(comp_id)
                .expect("Entity must possess CustomVelocity component");
            let read_vel = unsafe { *(comp_ptr.as_ptr() as *const CustomVelocity) };
            assert_eq!(read_vel, vel_data[i]);
        }

        let despawn_target = spawned_entities[0];
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            assert_eq!(
                unsafe { host_cmd_despawn(cmds.as_ffi_ptr(), despawn_target) },
                FfiResult::OK
            );
        }
        storage.apply_to_world(&mut world, &snap);

        assert!(
            world
                .get_entity(Entity::from_bits(despawn_target.0))
                .is_err()
        );
    }

    #[repr(C)]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CustomScore {
        points: u32,
        multiplier: u32,
    }

    unsafe extern "C" fn dynamic_score_mutator_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let exec_count = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let data_ptr = unsafe { host_iter_column_mutable(iter, 0) };
            assert!(!data_ptr.is_null());

            let stride = core::mem::size_of::<CustomScore>();
            for row in 0..count {
                let score_ptr = unsafe { data_ptr.add(row * stride) as *mut CustomScore };
                unsafe {
                    (*score_ptr).points += 100 * (*score_ptr).multiplier;
                }
            }
            exec_count.fetch_add(count, Ordering::Relaxed);
        }
        FfiResult::OK
    }

    #[test]
    fn test_dynamic_system_chunk_iteration_and_mutation() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let desc = ComponentDesc::new(
            c"dyn_life::CustomScore".as_ptr(),
            ComponentUuid::from_parts(
                "dyn_life::CustomScore",
                core::mem::size_of::<CustomScore>(),
                core::mem::align_of::<CustomScore>(),
                0,
            ),
            core::mem::size_of::<CustomScore>(),
            core::mem::align_of::<CustomScore>(),
        );

        let mut score_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut score_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let comp_id = snap.get_by_handle(score_h).unwrap().component_id;

        let mut storage = HostCommandsStorage::new();
        let mut entities = Vec::new();
        {
            let cmds = CommandHelper(&mut storage);
            let mut host_cmds = HostCommands::new(cmds.0, app.world().entity_allocator(), &snap);
            let raw_cmds = host_cmds.as_ffi_ptr();

            for i in 0..10u32 {
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                    FfiResult::OK
                );
                let score = CustomScore {
                    points: 10 * i,
                    multiplier: i + 1,
                };
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            ffi_e,
                            score_h,
                            (&score as *const CustomScore).cast::<u8>(),
                            core::mem::size_of::<CustomScore>(),
                        )
                    },
                    FfiResult::OK
                );
                entities.push(ffi_e);
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        struct CommandHelper<'a>(&'a mut HostCommandsStorage);

        let exec_count = Box::new(AtomicUsize::new(0));
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let system = GuestSystem::new(
            "score_mutator",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(score_h)],
            }],
            dynamic_score_mutator_system,
            (&*exec_count as *const AtomicUsize) as *mut c_void,
            world_reg,
        );
        app.add_systems(Update, system);
        app.update();

        assert_eq!(exec_count.load(Ordering::Relaxed), 10);

        for (i, &ffi_e) in entities.iter().enumerate() {
            let bevy_e = Entity::from_bits(ffi_e.0);
            let ptr = app.world().entity(bevy_e).get_by_id(comp_id).unwrap();
            let score = unsafe { *(ptr.as_ptr() as *const CustomScore) };
            let expected_points = (10 * i as u32) + 100 * (i as u32 + 1);
            assert_eq!(score.points, expected_points);
            assert_eq!(score.multiplier, (i as u32) + 1);
        }
    }

    unsafe extern "C" fn selective_change_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let selective_count = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            // Untracked mutable pointer does NOT dirty chunk ticks
            let untracked_ptr = unsafe { host_iter_column_mutable_untracked(iter, 0) };
            assert!(!untracked_ptr.is_null());

            // Mark only EVEN rows as changed via `host_iter_mark_changed_range`
            for row in 0..count {
                if row % 2 == 0 {
                    let mark_res = unsafe { host_iter_mark_changed_range(iter, 0, row, 1) };
                    assert_eq!(mark_res, FfiResult::OK);
                    selective_count.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_change_detection_tracking() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let desc = ComponentDesc::new(
            c"dyn_life::DynamicMana".as_ptr(),
            ComponentUuid::from_parts("dyn_life::DynamicMana", 4, 4, 0),
            4,
            4,
        );

        let mut mana_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut mana_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            for i in 0..6u32 {
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                    FfiResult::OK
                );
                let mana: u32 = 100 + i;
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            ffi_e,
                            mana_h,
                            (&mana as *const u32).cast::<u8>(),
                            4,
                        )
                    },
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        let selective_count = Box::new(AtomicUsize::new(0));
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "selective_change_sys",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(mana_h)],
            }],
            selective_change_system,
            (&*selective_count as *const AtomicUsize) as *mut c_void,
            world_reg,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(selective_count.load(Ordering::Relaxed), 3);
    }
}

mod column_bounds_and_zst_alignment {
    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, LoadContext,
        host_cmd_despawn, host_cmd_insert, host_cmd_spawn_empty, host_iter_column_info,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_mark_changed_range,
        host_iter_next_chunk, host_register_component, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamColumnInfo, BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc,
        ComponentUuid, FfiEntity, FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::prelude::World;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    unsafe extern "C" fn bounds_stress_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let executed = unsafe { &*(user_data as *const AtomicBool) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let col0_ro = unsafe { host_iter_column_readonly(iter, 0) };
            let col0_mut = unsafe { host_iter_column_mutable(iter, 0) };
            let mut info = MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(iter, 0, info.as_mut_ptr()) },
                FfiResult::OK
            );
            assert!(!col0_ro.is_null());
            assert!(!col0_mut.is_null());
            assert_eq!(unsafe { info.assume_init() }.size, 4);

            // Out-of-bounds column queries: index 1, index 32, index 99999 must return null / OUT_OF_RANGE
            assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
            assert!(unsafe { host_iter_column_readonly(iter, 32) }.is_null());
            assert!(unsafe { host_iter_column_readonly(iter, 99999) }.is_null());
            assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());
            assert!(unsafe { host_iter_column_mutable(iter, 32) }.is_null());
            assert!(unsafe { host_iter_column_mutable(iter, 99999) }.is_null());
            assert_eq!(
                unsafe { host_iter_column_info(iter, 1, info.as_mut_ptr()) },
                FfiResult::OUT_OF_RANGE
            );
            assert_eq!(
                unsafe { host_iter_column_info(iter, 99999, info.as_mut_ptr()) },
                FfiResult::OUT_OF_RANGE
            );

            // Out-of-bounds row & column indices in `host_iter_mark_changed_range` -> `OUT_OF_RANGE`
            assert_eq!(
                unsafe { host_iter_mark_changed_range(iter, 0, count + 100, 1) },
                FfiResult::OUT_OF_RANGE
            );
            assert_eq!(
                unsafe { host_iter_mark_changed_range(iter, 99, 0, 1) },
                FfiResult::OUT_OF_RANGE
            );

            executed.store(true, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    #[test]
    fn test_column_and_row_boundary_enforcement() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let desc = ComponentDesc::new(
            c"bounds_suite::BoundaryComp".as_ptr(),
            ComponentUuid::from_parts("bounds_suite::BoundaryComp", 4, 4, 0),
            4,
            4,
        );
        let mut comp_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut comp_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            let val: u32 = 777;
            for _ in 0..5 {
                let mut e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut e) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe {
                        host_cmd_insert(raw_cmds, e, comp_h, (&val as *const u32).cast::<u8>(), 4)
                    },
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        let executed = Box::new(AtomicBool::new(false));
        let reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "bounds_stress_sys",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(comp_h)],
            }],
            bounds_stress_callback,
            (&*executed as *const AtomicBool) as *mut c_void,
            reg,
        );
        app.add_systems(Update, sys);
        app.update();

        assert!(executed.load(Ordering::SeqCst));
    }

    #[repr(C, align(64))]
    #[derive(Copy, Clone, Debug, PartialEq)]
    struct CacheLineAlignedComp {
        data: [u64; 8],
    }

    #[test]
    fn test_zst_and_high_alignment_dynamic_components() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let mut zst_h = BamComponent::INVALID;
        let mut align_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let zst_desc = ComponentDesc::new(
                c"bounds_suite::ZstMarkerComp".as_ptr(),
                ComponentUuid::from_parts("bounds_suite::ZstMarkerComp", 0, 1, 0),
                0,
                1,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &zst_desc, &mut zst_h) },
                FfiResult::OK
            );

            let align_desc = ComponentDesc::new(
                c"bounds_suite::CacheLineAlignedComp".as_ptr(),
                ComponentUuid::from_parts("bounds_suite::CacheLineAlignedComp", 64, 64, 0),
                64,
                64,
            );
            assert_eq!(
                unsafe {
                    host_register_component(load_ctx.as_ffi_ptr(), &align_desc, &mut align_h)
                },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let aligned_val = CacheLineAlignedComp {
            data: [0x1122_3344_5566_7788; 8],
        };
        let ffi_e = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            let mut e = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut e) },
                FfiResult::OK
            );
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        e,
                        zst_h,
                        core::ptr::NonNull::<u8>::dangling().as_ptr(),
                        0,
                    )
                },
                FfiResult::OK
            );
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        e,
                        align_h,
                        (&aligned_val as *const CacheLineAlignedComp).cast::<u8>(),
                        64,
                    )
                },
                FfiResult::OK
            );
            e
        };
        storage.apply_to_world(&mut world, &snap);

        let bevy_entity = Entity::from_bits(ffi_e.0);
        let ent_ref = world.entity(bevy_entity);
        let zst_id = snap.get_by_handle(zst_h).unwrap().component_id;
        let align_id = snap.get_by_handle(align_h).unwrap().component_id;

        assert!(ent_ref.contains_id(zst_id));
        assert!(ent_ref.contains_id(align_id));

        let ptr = ent_ref.get_by_id(align_id).unwrap();
        assert_eq!((ptr.as_ptr() as usize) % 64, 0);
        let read_val = unsafe { *(ptr.as_ptr() as *const CacheLineAlignedComp) };
        assert_eq!(read_val, aligned_val);
    }

    #[test]
    fn test_deferred_command_error_conditions() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let mut rb_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let desc = ComponentDesc::new(
                c"bounds_suite::RigidBody".as_ptr(),
                ComponentUuid::from_parts("bounds_suite::RigidBody", 16, 8, 0),
                16,
                8,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut rb_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let raw_cmds = cmds.as_ffi_ptr();

        let mut ffi_e = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
            FfiResult::OK
        );

        // 1. Length mismatch on insertion (expected 16 bytes, passed 8 bytes)
        let small_data = [0u8; 8];
        assert_eq!(
            unsafe { host_cmd_insert(raw_cmds, ffi_e, rb_h, small_data.as_ptr(), 8) },
            FfiResult::INVALID_ARGUMENT
        );

        // 2. Unknown component handle on insertion
        let valid_data = [0u8; 16];
        assert_eq!(
            unsafe {
                host_cmd_insert(
                    raw_cmds,
                    ffi_e,
                    BamComponent::from_raw(999),
                    valid_data.as_ptr(),
                    16,
                )
            },
            FfiResult::INVALID_COMPONENT
        );

        // 3. Null queue handle
        let mut out_null = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(core::ptr::null_mut(), &mut out_null) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_despawn(core::ptr::null_mut(), ffi_e) },
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            unsafe { host_cmd_insert(core::ptr::null_mut(), ffi_e, rb_h, valid_data.as_ptr(), 16) },
            FfiResult::NULL_POINTER
        );
    }
}

mod empirical_pointer_arithmetic_and_decoupling {
    use bevy_abi_mod_host::plugin::AbiModPlugin;
    use bevy_abi_mod_host_sys::registry::ModRegistry;
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, LoadContext,
        host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty, host_iter_column_info,
        host_iter_column_mutable, host_iter_column_mutable_untracked, host_iter_column_readonly,
        host_iter_entities, host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset,
        host_register_component, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamColumnInfo, BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc,
        ComponentUuid, FfiEntity, FfiResult,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::entity::Entity;
    use bevy_ecs::system::System;
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

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
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let (h1, h2, h4, h8, h16, h32, h64) = {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            macro_rules! register_type {
                ($t:ty, $name:literal) => {{
                    let name_cstr = concat!($name, "\0").as_bytes();
                    let desc = ComponentDesc::new(
                        name_cstr.as_ptr().cast(),
                        ComponentUuid::from_parts(
                            $name,
                            core::mem::size_of::<$t>(),
                            core::mem::align_of::<$t>(),
                            0,
                        ),
                        core::mem::size_of::<$t>(),
                        core::mem::align_of::<$t>(),
                    );
                    let mut h = BamComponent::INVALID;
                    let res =
                        unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut h) };
                    assert_eq!(res, FfiResult::OK, "Failed to register {}", $name);
                    h
                }};
            }

            let handles = (
                register_type!(CompAlign1, "matrix::CompAlign1"),
                register_type!(CompAlign2, "matrix::CompAlign2"),
                register_type!(CompAlign4, "matrix::CompAlign4"),
                register_type!(CompAlign8, "matrix::CompAlign8"),
                register_type!(CompAlign16, "matrix::CompAlign16"),
                register_type!(CompAlign32, "matrix::CompAlign32"),
                register_type!(CompAlign64, "matrix::CompAlign64"),
            );
            load_ctx.finish_init();
            handles
        };

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        const ENTITY_COUNT: usize = 16;
        let mut entities = Vec::new();

        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();

            for i in 0..ENTITY_COUNT {
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

                let comps = [h1, h2, h4, h8, h16, h32, h64];
                let ptrs = [
                    (&c1 as *const CompAlign1).cast::<u8>(),
                    (&c2 as *const CompAlign2).cast::<u8>(),
                    (&c4 as *const CompAlign4).cast::<u8>(),
                    (&c8 as *const CompAlign8).cast::<u8>(),
                    (&c16 as *const CompAlign16).cast::<u8>(),
                    (&c32 as *const CompAlign32).cast::<u8>(),
                    (&c64 as *const CompAlign64).cast::<u8>(),
                ];
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe {
                        host_cmd_spawn(raw_cmds, comps.as_ptr(), ptrs.as_ptr(), 7, &mut ffi_e)
                    },
                    FfiResult::OK
                );
                entities.push(ffi_e);
            }
        }

        storage.apply_to_world(&mut world, &snap);

        for (i, ffi_e) in entities.iter().enumerate().take(ENTITY_COUNT) {
            let bevy_e = Entity::from_bits(ffi_e.0);
            let e_ref = world.entity(bevy_e);

            macro_rules! check_val {
                ($t:ty, $h:expr, $expected:expr) => {{
                    let comp_id = snap.get_by_handle($h).unwrap().component_id;
                    let ptr = e_ref.get_by_id(comp_id).unwrap().as_ptr();
                    assert_eq!(ptr as usize % core::mem::align_of::<$t>(), 0);
                    let read_val = unsafe { *(ptr as *const $t) };
                    assert_eq!(read_val, $expected);
                }};
            }

            check_val!(CompAlign1, h1, CompAlign1 { val: i as u8 });
            check_val!(CompAlign2, h2, CompAlign2 { val: i as u16 * 10 });
            check_val!(
                CompAlign4,
                h4,
                CompAlign4 {
                    val: i as u32 * 100
                }
            );
            check_val!(
                CompAlign8,
                h8,
                CompAlign8 {
                    val: i as u64 * 1000
                }
            );
            check_val!(
                CompAlign16,
                h16,
                CompAlign16 {
                    val: [i as u64, i as u64 * 2]
                }
            );
            check_val!(CompAlign32, h32, CompAlign32 { val: [i as u64; 4] });
            check_val!(CompAlign64, h64, CompAlign64 { val: [i as u64; 8] });
        }
    }

    unsafe extern "C" fn dynamic_chunk_pointer_arithmetic_verifier(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let verified = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        if unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let p1 = unsafe { host_iter_column_mutable(iter, 0) };
            let p16 = unsafe { host_iter_column_mutable(iter, 1) };
            let p64 = unsafe { host_iter_column_mutable(iter, 2) };
            assert!(!p1.is_null() && !p16.is_null() && !p64.is_null());

            let mut i0 = MaybeUninit::<BamColumnInfo>::uninit();
            let mut i1 = MaybeUninit::<BamColumnInfo>::uninit();
            let mut i2 = MaybeUninit::<BamColumnInfo>::uninit();
            unsafe {
                assert_eq!(
                    host_iter_column_info(iter, 0, i0.as_mut_ptr()),
                    FfiResult::OK
                );
                assert_eq!(
                    host_iter_column_info(iter, 1, i1.as_mut_ptr()),
                    FfiResult::OK
                );
                assert_eq!(
                    host_iter_column_info(iter, 2, i2.as_mut_ptr()),
                    FfiResult::OK
                );
            }
            let stride1 = unsafe { i0.assume_init() }.size;
            let stride16 = unsafe { i1.assume_init() }.size;
            let stride64 = unsafe { i2.assume_init() }.size;

            assert_eq!(stride1, core::mem::size_of::<CompAlign1>());
            assert_eq!(stride16, core::mem::size_of::<CompAlign16>());
            assert_eq!(stride64, core::mem::size_of::<CompAlign64>());

            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());

            for row in 0..count {
                let ptr1_row = unsafe { p1.add(row * stride1) };
                let ptr16_row = unsafe { p16.add(row * stride16) };
                let ptr64_row = unsafe { p64.add(row * stride64) };

                assert_eq!(ptr1_row as usize % core::mem::align_of::<CompAlign1>(), 0);
                assert_eq!(ptr16_row as usize % core::mem::align_of::<CompAlign16>(), 0);
                assert_eq!(ptr64_row as usize % core::mem::align_of::<CompAlign64>(), 0);

                unsafe {
                    let c1 = &mut *(ptr1_row as *mut CompAlign1);
                    c1.val = c1.val.wrapping_add(1);

                    let c16 = &mut *(ptr16_row as *mut CompAlign16);
                    c16.val[0] += 500;

                    let c64 = &mut *(ptr64_row as *mut CompAlign64);
                    c64.val[0] += 1000;
                }

                verified.fetch_add(1, Ordering::Relaxed);
            }

            // Reset and verify chunk cursor rewinds to same data pointer (`iter_reset`)
            assert_eq!(unsafe { host_iter_reset(iter) }, FfiResult::OK);
            let mut re_count = 0usize;
            assert!(unsafe { host_iter_next_chunk(iter, &mut re_count) }.get());
            assert_eq!(re_count, count);
            let re_p1 = unsafe { host_iter_column_readonly(iter, 0) };
            assert_eq!(re_p1, p1.cast_const());
        }

        FfiResult::OK
    }

    #[test]
    fn test_chunk_pointer_arithmetic_and_reset() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let (h1, h16, h64) = {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            macro_rules! register_type {
                ($t:ty, $name:literal) => {{
                    let name_cstr = concat!($name, "\0").as_bytes();
                    let desc = ComponentDesc::new(
                        name_cstr.as_ptr().cast(),
                        ComponentUuid::from_parts(
                            $name,
                            core::mem::size_of::<$t>(),
                            core::mem::align_of::<$t>(),
                            0,
                        ),
                        core::mem::size_of::<$t>(),
                        core::mem::align_of::<$t>(),
                    );
                    let mut h = BamComponent::INVALID;
                    assert_eq!(
                        unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut h) },
                        FfiResult::OK
                    );
                    h
                }};
            }
            let handles = (
                register_type!(CompAlign1, "arith::ChunkIterComp1"),
                register_type!(CompAlign16, "arith::ChunkIterComp16"),
                register_type!(CompAlign64, "arith::ChunkIterComp64"),
            );
            load_ctx.finish_init();
            handles
        };

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut entities = Vec::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            for i in 0..8usize {
                let c1 = CompAlign1 { val: i as u8 };
                let c16 = CompAlign16 {
                    val: [i as u64, i as u64 * 2],
                };
                let c64 = CompAlign64 { val: [i as u64; 8] };
                let comps = [h1, h16, h64];
                let ptrs = [
                    (&c1 as *const CompAlign1).cast::<u8>(),
                    (&c16 as *const CompAlign16).cast::<u8>(),
                    (&c64 as *const CompAlign64).cast::<u8>(),
                ];
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe {
                        host_cmd_spawn(raw_cmds, comps.as_ptr(), ptrs.as_ptr(), 3, &mut ffi_e)
                    },
                    FfiResult::OK
                );
                entities.push(ffi_e);
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        let verified = Box::new(AtomicUsize::new(0));
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "pointer_arith_verifier",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::write(h1),
                    BamTermDesc::write(h16),
                    BamTermDesc::write(h64),
                ],
            }],
            dynamic_chunk_pointer_arithmetic_verifier,
            (&*verified as *const AtomicUsize) as *mut c_void,
            world_reg,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(verified.load(Ordering::SeqCst), 8);

        let id1 = snap.get_by_handle(h1).unwrap().component_id;
        let id16 = snap.get_by_handle(h16).unwrap().component_id;
        let id64 = snap.get_by_handle(h64).unwrap().component_id;

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

    unsafe extern "C" fn zst_system_callback(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let zst_count = unsafe { &*(user_data as *const AtomicUsize) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let p_zst1 = unsafe { host_iter_column_readonly(iter, 0) };
            let p_zst16 = unsafe { host_iter_column_mutable(iter, 1) };
            let p_comp4 = unsafe { host_iter_column_mutable(iter, 2) };
            assert!(!p_zst1.is_null() && !p_zst16.is_null() && !p_comp4.is_null());

            let mut i0 = MaybeUninit::<BamColumnInfo>::uninit();
            let mut i1 = MaybeUninit::<BamColumnInfo>::uninit();
            let mut i2 = MaybeUninit::<BamColumnInfo>::uninit();
            unsafe {
                assert_eq!(
                    host_iter_column_info(iter, 0, i0.as_mut_ptr()),
                    FfiResult::OK
                );
                assert_eq!(
                    host_iter_column_info(iter, 1, i1.as_mut_ptr()),
                    FfiResult::OK
                );
                assert_eq!(
                    host_iter_column_info(iter, 2, i2.as_mut_ptr()),
                    FfiResult::OK
                );
                assert_eq!(i0.assume_init().size, 0, "ZST size must be 0");
                assert_eq!(i1.assume_init().size, 0, "ZST size must be 0");
                assert_eq!(i2.assume_init().size, 4);
            }

            zst_count.fetch_add(count, Ordering::Relaxed);
        }
        FfiResult::OK
    }

    #[test]
    fn test_zst_components_lifecycle_and_iteration() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let mut h_zst1 = BamComponent::INVALID;
        let mut h_zst16 = BamComponent::INVALID;
        let mut h_c4 = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            let desc_zst1 = ComponentDesc::new(
                c"zst_life::ZstAlign1".as_ptr(),
                ComponentUuid::from_parts("zst_life::ZstAlign1", 0, 1, 0),
                0,
                1,
            );
            let desc_zst16 = ComponentDesc::new(
                c"zst_life::ZstAlign16".as_ptr(),
                ComponentUuid::from_parts("zst_life::ZstAlign16", 0, 16, 0),
                0,
                16,
            );
            let desc_c4 = ComponentDesc::new(
                c"zst_life::PayloadC4".as_ptr(),
                ComponentUuid::from_parts("zst_life::PayloadC4", 4, 4, 0),
                4,
                4,
            );
            unsafe {
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc_zst1, &mut h_zst1),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc_zst16, &mut h_zst16),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(load_ctx.as_ffi_ptr(), &desc_c4, &mut h_c4),
                    FfiResult::OK
                );
            }
            load_ctx.finish_init();
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            for i in 0..5u32 {
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                    FfiResult::OK
                );
                let dummy_byte = 0u8;
                assert_eq!(
                    unsafe { host_cmd_insert(raw_cmds, ffi_e, h_zst1, &dummy_byte, 0) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe { host_cmd_insert(raw_cmds, ffi_e, h_zst16, core::ptr::null(), 0) },
                    FfiResult::OK
                );
                let payload = i * 7;
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            raw_cmds,
                            ffi_e,
                            h_c4,
                            (&payload as *const u32).cast::<u8>(),
                            4,
                        )
                    },
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        let zst_count = Box::new(AtomicUsize::new(0));
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "zst_system",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::read(h_zst1),
                    BamTermDesc::write(h_zst16),
                    BamTermDesc::write(h_c4),
                ],
            }],
            zst_system_callback,
            (&*zst_count as *const AtomicUsize) as *mut c_void,
            world_reg,
        );
        app.add_systems(Update, sys);
        app.update();

        assert_eq!(zst_count.load(Ordering::SeqCst), 5);
    }

    unsafe extern "C" fn multi_chunk_collector_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let records = unsafe { &*(user_data as *const Mutex<Vec<(u64, u32)>>) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let p_shared = unsafe { host_iter_column_readonly(iter, 0) };
            assert!(!p_shared.is_null());
            let mut info = MaybeUninit::<BamColumnInfo>::uninit();
            assert_eq!(
                unsafe { host_iter_column_info(iter, 0, info.as_mut_ptr()) },
                FfiResult::OK
            );
            let stride = unsafe { info.assume_init() }.size;
            assert_eq!(stride, 4);

            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());

            let mut guard = records.lock().unwrap();
            for row in 0..count {
                let entity_id = unsafe { (*entities_ptr.add(row)).0 };
                let val = unsafe { *(p_shared.add(row * stride) as *const u32) };
                guard.push((entity_id, val));
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_multi_archetype_chunks_contiguous_traversal() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        let mut h_shared = BamComponent::INVALID;
        let mut h_ma = BamComponent::INVALID;
        let mut h_mb = BamComponent::INVALID;
        let mut h_mc = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                app.world().resource::<ModRegistry>().clone(),
                Arc::new(AtomicBool::new(true)),
                app.world_mut() as *mut World,
                None,
            );
            for (name, cstr, out) in [
                (
                    "contig::SharedComp",
                    c"contig::SharedComp".as_ptr(),
                    &mut h_shared,
                ),
                ("contig::MarkerA", c"contig::MarkerA".as_ptr(), &mut h_ma),
                ("contig::MarkerB", c"contig::MarkerB".as_ptr(), &mut h_mb),
                ("contig::MarkerC", c"contig::MarkerC".as_ptr(), &mut h_mc),
            ] {
                let desc = ComponentDesc::new(cstr, ComponentUuid::from_parts(name, 4, 4, 0), 4, 4);
                assert_eq!(
                    unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, out) },
                    FfiResult::OK
                );
            }
            load_ctx.finish_init();
        }

        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let mut expected_map = HashMap::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();

            for (marker_h, base, n) in [(h_ma, 100u32, 3u32), (h_mb, 200, 4), (h_mc, 300, 5)] {
                for i in 0..n {
                    let val = base + i;
                    let comps = [h_shared, marker_h];
                    let ptrs = [
                        (&val as *const u32).cast::<u8>(),
                        (&val as *const u32).cast::<u8>(),
                    ];
                    let mut ffi_e = FfiEntity::NULL;
                    assert_eq!(
                        unsafe {
                            host_cmd_spawn(raw_cmds, comps.as_ptr(), ptrs.as_ptr(), 2, &mut ffi_e)
                        },
                        FfiResult::OK
                    );
                    expected_map.insert(ffi_e.0, val);
                }
            }
        }
        storage.apply_to_world(app.world_mut(), &snap);

        let records: Arc<Mutex<Vec<(u64, u32)>>> = Arc::new(Mutex::new(Vec::new()));
        let world_reg = app.world().resource::<ModRegistry>().clone();
        let sys = GuestSystem::new(
            "multi_chunk_collector",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(h_shared)],
            }],
            multi_chunk_collector_system,
            Arc::as_ptr(&records) as *mut c_void,
            world_reg,
        );
        app.add_systems(Update, sys);
        app.update();

        let collected = records.lock().unwrap().clone();
        assert_eq!(collected.len(), 12);
        for (e_id, val) in collected {
            assert_eq!(Some(&val), expected_map.get(&e_id));
        }
    }

    unsafe extern "C" fn fine_grained_change_marker_system(
        sys_ctx: *mut BamSystemCtx,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let _ptr = unsafe { host_iter_column_mutable_untracked(iter, 0) };
            if count > 1 {
                assert_eq!(
                    unsafe { host_iter_mark_changed_range(iter, 0, 1, 1) },
                    FfiResult::OK
                );
            }
        }
        FfiResult::OK
    }

    #[test]
    fn test_native_bevy_change_detection_parity() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let mut comp_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let desc = ComponentDesc::new(
                c"parity::TickTrackComp".as_ptr(),
                ComponentUuid::from_parts("parity::TickTrackComp", 4, 4, 0),
                4,
                4,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut comp_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let comp_id = snap.get_by_handle(comp_h).unwrap().component_id;
        let mut storage = HostCommandsStorage::new();
        let mut entities = Vec::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            for i in 0..4u32 {
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe {
                        host_cmd_insert(raw_cmds, ffi_e, comp_h, (&i as *const u32).cast::<u8>(), 4)
                    },
                    FfiResult::OK
                );
                entities.push(Entity::from_bits(ffi_e.0));
            }
        }
        storage.apply_to_world(&mut world, &snap);

        let frame1_tick = world.change_tick();
        let world_reg = world.resource::<ModRegistry>().clone();
        let mut sys = GuestSystem::new(
            "tick_marker",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(comp_h)],
            }],
            fine_grained_change_marker_system,
            core::ptr::null_mut(),
            world_reg,
        );
        sys.initialize(&mut world);

        world.increment_change_tick();
        unsafe {
            sys.run_unsafe((), world.as_unsafe_world_cell()).unwrap();
        }
        let run_tick = sys.get_last_run();

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

        assert_eq!(ticks_e1.changed, run_tick);
        assert_ne!(ticks_e0.changed, run_tick);
        assert_ne!(ticks_e2.changed, run_tick);
        assert_ne!(ticks_e3.changed, run_tick);

        assert!(ticks_e1.is_changed(frame1_tick, run_tick));
        assert!(!ticks_e0.is_changed(frame1_tick, run_tick));
    }

    #[test]
    fn test_foreign_stack_memory_overwrite_decoupling() {
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let mut comp_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let desc = ComponentDesc::new(
                c"decouple::StackDecoupleComp".as_ptr(),
                ComponentUuid::from_parts("decouple::StackDecoupleComp", 32, 16, 0),
                32,
                16,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut comp_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let comp_id = snap.get_by_handle(comp_h).unwrap().component_id;
        let mut storage = HostCommandsStorage::new();
        let ffi_e = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            let mut e = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut e) },
                FfiResult::OK
            );

            fn insert_and_smash_stack(
                cmds_ptr: *mut bevy_abi_mod_sys::BamCommands,
                entity: FfiEntity,
                comp: BamComponent,
            ) {
                let mut stack_buf = [0x55u8; 32];
                stack_buf[0] = 0xAA;
                stack_buf[31] = 0xBB;
                assert_eq!(
                    unsafe { host_cmd_insert(cmds_ptr, entity, comp, stack_buf.as_ptr(), 32) },
                    FfiResult::OK
                );
                stack_buf.fill(0x00);
                core::hint::black_box(&stack_buf);
            }

            insert_and_smash_stack(raw_cmds, e, comp_h);
            e
        };

        storage.apply_to_world(&mut world, &snap);

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
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let mut comp_h = BamComponent::INVALID;
        {
            let mut load_ctx = LoadContext::with_world_and_mod_id(
                registry,
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                None,
            );
            let desc = ComponentDesc::new(
                c"decouple::HeavyAllocComp".as_ptr(),
                ComponentUuid::from_parts("decouple::HeavyAllocComp", 256, 32, 0),
                256,
                32,
            );
            assert_eq!(
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut comp_h) },
                FfiResult::OK
            );
            load_ctx.finish_init();
        }

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        {
            let mut storage = HostCommandsStorage::new();
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();

            let dummy_data = [0x77u8; 256];
            for _ in 0..500 {
                let mut ffi_e = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(raw_cmds, &mut ffi_e) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe { host_cmd_insert(raw_cmds, ffi_e, comp_h, dummy_data.as_ptr(), 256) },
                    FfiResult::OK
                );
            }
            // `storage` is dropped here without `apply_to_world`
        }
    }
}

mod padded_and_entity_layout {
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        HostCommands, HostCommandsStorage, LoadContext, host_cmd_insert, host_cmd_spawn,
        host_cmd_spawn_empty, host_register_component,
    };
    use bevy_abi_mod_sys::{BamComponent, ComponentDesc, ComponentUuid, FfiEntity, FfiResult};
    use bevy_app::App;
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::world::World;
    use core::mem::MaybeUninit;
    use core::sync::atomic::AtomicBool;
    use std::sync::Arc;

    #[repr(C)]
    #[derive(Component, ModComponent, Copy, Clone, Debug, PartialEq, Eq)]
    #[mod_component(name = "ffi_layout::PaddedLayoutPair")]
    struct PaddedLayoutPair {
        tag: u8,
        // 7 padding bytes at offsets 1..8
        value: u64,
    }

    #[test]
    fn padded_struct_insert_preserves_fields_with_uninit_padding() {
        assert_eq!(core::mem::size_of::<PaddedLayoutPair>(), 16);
        assert_eq!(core::mem::align_of::<PaddedLayoutPair>(), 8);

        // Construct a `PaddedLayoutPair` leaving its 7 padding bytes genuinely uninitialized.
        let mut raw_slot = MaybeUninit::<PaddedLayoutPair>::uninit();
        unsafe {
            let base = raw_slot.as_mut_ptr();
            (&raw mut (*base).tag).write(0x5A);
            (&raw mut (*base).value).write(0xDEAD_BEEF_CAFE_BABEu64);
        }
        let raw_bytes_ptr = raw_slot.as_ptr().cast::<u8>();

        // 1. Static host-registered component insert + batched spawn paths
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let padded_handle = app
            .register_mod_component::<PaddedLayoutPair>(ComponentOptions::new())
            .expect("register PaddedLayoutPair");

        let (e_static, e_static_batch) = {
            let world = app.world_mut();
            let snap = world.resource::<ModRegistry>().snapshot_arc();
            let mut storage = HostCommandsStorage::new();
            let (e1, e2) = {
                let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                let cmds_ptr = cmds.as_ffi_ptr();

                let mut e1 = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(cmds_ptr, &mut e1) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe {
                        host_cmd_insert(
                            cmds_ptr,
                            e1,
                            padded_handle,
                            raw_bytes_ptr,
                            core::mem::size_of::<PaddedLayoutPair>(),
                        )
                    },
                    FfiResult::OK
                );

                let comps = [padded_handle];
                let ptrs = [raw_bytes_ptr];
                let mut e2 = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn(cmds_ptr, comps.as_ptr(), ptrs.as_ptr(), 1, &mut e2) },
                    FfiResult::OK
                );
                (e1, e2)
            };
            storage.apply_to_world(world, &snap);
            (
                Entity::from_bits(e1.to_bits()),
                Entity::from_bits(e2.to_bits()),
            )
        };

        for ent in [e_static, e_static_batch] {
            let got = app
                .world()
                .get::<PaddedLayoutPair>(ent)
                .expect("PaddedLayoutPair inserted via static command path");
            assert_eq!(got.tag, 0x5A);
            assert_eq!(got.value, 0xDEAD_BEEF_CAFE_BABEu64);
        }

        // 2. Dynamic guest-registered component insert + batched spawn paths
        let mut dyn_handle = BamComponent::INVALID;
        {
            let world = app.world_mut();
            let reg = world.resource::<ModRegistry>().clone();
            let is_active = Arc::new(AtomicBool::new(true));
            let mut load_ctx =
                LoadContext::with_world_and_mod_id(reg, is_active, world as *mut World, None);

            let desc = ComponentDesc::new(
                c"ffi_layout::DynamicPaddedLayoutPair".as_ptr(),
                ComponentUuid::from_parts("ffi_layout::DynamicPaddedLayoutPair", 16, 8, 0),
                16,
                8,
            );
            let rc =
                unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut dyn_handle) };
            assert_eq!(rc, FfiResult::OK);
            load_ctx.finish_init();
        }

        let dyn_comp_id = app
            .world()
            .resource::<ModRegistry>()
            .get_by_handle(dyn_handle)
            .expect("DynamicPaddedLayoutPair registered")
            .component_id;

        let (e_dyn, e_dyn_batch) = {
            let world = app.world_mut();
            let snap = world.resource::<ModRegistry>().snapshot_arc();
            let mut storage = HostCommandsStorage::new();
            let (e1, e2) = {
                let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                let cmds_ptr = cmds.as_ffi_ptr();

                let mut e1 = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn_empty(cmds_ptr, &mut e1) },
                    FfiResult::OK
                );
                assert_eq!(
                    unsafe { host_cmd_insert(cmds_ptr, e1, dyn_handle, raw_bytes_ptr, 16) },
                    FfiResult::OK
                );

                let comps = [dyn_handle];
                let ptrs = [raw_bytes_ptr];
                let mut e2 = FfiEntity::NULL;
                assert_eq!(
                    unsafe { host_cmd_spawn(cmds_ptr, comps.as_ptr(), ptrs.as_ptr(), 1, &mut e2) },
                    FfiResult::OK
                );
                (e1, e2)
            };
            storage.apply_to_world(world, &snap);
            (
                Entity::from_bits(e1.to_bits()),
                Entity::from_bits(e2.to_bits()),
            )
        };

        for ent in [e_dyn, e_dyn_batch] {
            let entity_ref = app.world().entity(ent);
            let untyped_ptr = entity_ref
                .get_by_id(dyn_comp_id)
                .expect("DynamicPaddedLayoutPair present on entity");
            unsafe {
                let base = untyped_ptr.as_ptr();
                assert_eq!(core::ptr::read(base), 0x5A);
                assert_eq!(
                    core::ptr::read_unaligned(base.add(8).cast::<u64>()),
                    0xDEAD_BEEF_CAFE_BABEu64
                );
            }
        }
    }

    #[test]
    fn entity_and_ffi_entity_bit_representation_parity() {
        assert_eq!(
            core::mem::size_of::<Entity>(),
            core::mem::size_of::<FfiEntity>()
        );
        assert_eq!(
            core::mem::align_of::<Entity>(),
            core::mem::align_of::<FfiEntity>()
        );

        let mut world = World::new();
        let mut entities = Vec::new();
        for _ in 0..32 {
            entities.push(world.spawn_empty().id());
        }
        for e in entities.drain(..16).collect::<Vec<_>>() {
            world.despawn(e);
        }
        for _ in 0..32 {
            entities.push(world.spawn_empty().id());
        }

        for entity in entities {
            let raw_transmuted = unsafe { core::mem::transmute_copy::<Entity, u64>(&entity) };
            assert_eq!(raw_transmuted, entity.to_bits());
            let ffi = FfiEntity::from_bits(entity.to_bits());
            assert_eq!(ffi.to_bits(), raw_transmuted);
            assert_eq!(Entity::try_from_bits(ffi.to_bits()), Some(entity));
        }
    }
}
