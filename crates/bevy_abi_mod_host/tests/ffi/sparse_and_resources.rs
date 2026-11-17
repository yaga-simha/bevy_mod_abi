//! SparseSet columns, Optional/Changed/Added filters, ECS resources,
//! opaque VTables, and component collisions.

mod sparse_soundness_and_clone_semantics {
    use crate::support::{NameTag, Position, Velocity};
    use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
    use bevy_abi_mod_host_sys::{
        BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, LoadContext,
        host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_column_rows_readonly,
        host_iter_entities, host_iter_next_chunk, host_iter_reset, host_register_component,
        host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc, ComponentUuid, FfiEntity,
        FfiResult, ModComponent as _,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::{Changed, With};
    use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleBuildSettings};
    use bevy_ecs::world::World;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Arc;

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    #[mod_component(name = "sparse_test::SparseTag")]
    pub struct SparseTag {
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    #[mod_component(name = "sparse_test::SparseShield")]
    pub struct SparseShield {
        pub strength: f32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[mod_component(name = "sparse_test::FlaggedComp")]
    pub struct FlaggedComp {
        pub active: bool,
    }

    // ============================================================================
    // 1. D1: SparseSet Filters (`Without<Sparse>` / `With<Sparse>`) on Shared Table
    // ============================================================================

    unsafe extern "C" fn add_ten_to_position_x(
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
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Position;
            assert!(!pos_ptr.is_null());
            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());
            for row in 0..count {
                unsafe {
                    let _e = *entities_ptr.add(row);
                    (*pos_ptr.add(row)).x += 10.0;
                }
            }
        }
        FfiResult::OK
    }

    #[test]
    fn without_sparse_filter_does_not_touch_sparse_entities_in_shared_table() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let sparse_tag_h = app
            .register_mod_component::<SparseTag>(ComponentOptions::new())
            .unwrap();

        // Strict ambiguity detection: native `Query<&mut Position, With<SparseTag>>` and dynamic
        // `WRITE Position + WITHOUT SparseTag` are disjoint and must have zero schedule conflicts!
        app.edit_schedule(Update, |schedule| {
            schedule.set_build_settings(ScheduleBuildSettings {
                ambiguity_detection: bevy_ecs::schedule::LogLevel::Error,
                ..Default::default()
            });
        });

        let e_sparse_1 = app
            .world_mut()
            .spawn((
                Position {
                    x: 1.0,
                    y: 0.0,
                    z: 0.0,
                },
                SparseTag { code: 7 },
            ))
            .id();
        let e_plain_1 = app
            .world_mut()
            .spawn(Position {
                x: 100.0,
                y: 0.0,
                z: 0.0,
            })
            .id();
        let e_plain_2 = app
            .world_mut()
            .spawn(Position {
                x: 200.0,
                y: 0.0,
                z: 0.0,
            })
            .id();
        let e_sparse_2 = app
            .world_mut()
            .spawn((
                Position {
                    x: 2.0,
                    y: 0.0,
                    z: 0.0,
                },
                SparseTag { code: 9 },
            ))
            .id();

        let t_sparse = app.world().entity(e_sparse_1).location().table_id;
        let t_plain = app.world().entity(e_plain_1).location().table_id;
        assert_eq!(
            t_sparse, t_plain,
            "SparseSet component must leave entities in the same dense Table"
        );

        let reg = app.world().resource::<ModRegistry>().clone();
        let dyn_without_sparse = GuestSystem::new(
            "dyn_without_sparse",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::write(pos_h),
                    BamTermDesc::without(sparse_tag_h),
                ],
            }],
            add_ten_to_position_x,
            core::ptr::null_mut(),
            reg,
        );

        fn native_with_sparse(mut q: bevy_ecs::system::Query<&mut Position, With<SparseTag>>) {
            for mut pos in &mut q {
                pos.y += 5.0;
            }
        }

        app.add_systems(Update, (dyn_without_sparse, native_with_sparse));
        app.update();

        assert_eq!(
            app.world().get::<Position>(e_sparse_1).unwrap(),
            &Position {
                x: 1.0,
                y: 5.0,
                z: 0.0
            }
        );
        assert_eq!(
            app.world().get::<Position>(e_sparse_2).unwrap(),
            &Position {
                x: 2.0,
                y: 5.0,
                z: 0.0
            }
        );
        assert_eq!(
            app.world().get::<Position>(e_plain_1).unwrap(),
            &Position {
                x: 110.0,
                y: 0.0,
                z: 0.0
            }
        );
        assert_eq!(
            app.world().get::<Position>(e_plain_2).unwrap(),
            &Position {
                x: 210.0,
                y: 0.0,
                z: 0.0
            }
        );

        #[derive(Component)]
        struct ExtraDenseMarker;
        let e_new_arch = app
            .world_mut()
            .spawn((
                Position {
                    x: 300.0,
                    y: 0.0,
                    z: 0.0,
                },
                ExtraDenseMarker,
            ))
            .id();
        app.update();
        assert_eq!(app.world().get::<Position>(e_new_arch).unwrap().x, 310.0);
    }

    // ============================================================================
    // 2. SparseSet Data Columns (`rows_readonly` / `rows_mutable`) + Change Detection
    // ============================================================================

    struct SparseRwState {
        pass_counter: AtomicUsize,
        should_mutate: AtomicBool,
    }

    unsafe extern "C" fn transfer_pos_x_to_sparse_shield(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let state = unsafe { &*(user_data as *const SparseRwState) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        // Pass 1: count rows via `host_iter_column_rows_readonly` and verify dense column fns return null on SparseSet
        let mut seen_first_pass = 0usize;
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
            assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());
            let shield_rows_ro = unsafe { host_iter_column_rows_readonly(iter, 1) };
            assert!(!shield_rows_ro.is_null());
            seen_first_pass += count;
        }
        if !state.should_mutate.load(Ordering::SeqCst) {
            return FfiResult::OK;
        }

        assert_eq!(unsafe { host_iter_reset(iter) }, FfiResult::OK);

        let mut mutated = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
            let shield_rows = unsafe { host_iter_column_rows_mutable(iter, 1) };
            assert!(!pos_ptr.is_null());
            assert!(!shield_rows.is_null());
            for row in 0..count {
                unsafe {
                    let s_ptr = (*shield_rows.add(row)).cast::<SparseShield>();
                    (*s_ptr).strength += (*pos_ptr.add(row)).x;
                }
                mutated += 1;
            }
        }
        assert_eq!(seen_first_pass, mutated);
        state.pass_counter.fetch_add(mutated, Ordering::SeqCst);
        FfiResult::OK
    }

    #[test]
    fn sparse_set_read_write_columns_and_changed_ticks_work_end_to_end() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();
        let shield_h = app
            .register_mod_component::<SparseShield>(ComponentOptions::new())
            .unwrap();

        let e1 = app
            .world_mut()
            .spawn((
                Position {
                    x: 15.0,
                    y: 0.0,
                    z: 0.0,
                },
                SparseShield { strength: 100.0 },
            ))
            .id();
        let e_unmatched = app
            .world_mut()
            .spawn(Position {
                x: 999.0,
                y: 0.0,
                z: 0.0,
            })
            .id();
        let e2 = app
            .world_mut()
            .spawn((
                Position {
                    x: 25.0,
                    y: 0.0,
                    z: 0.0,
                },
                SparseShield { strength: 200.0 },
            ))
            .id();

        let state = Arc::new(SparseRwState {
            pass_counter: AtomicUsize::new(0),
            should_mutate: AtomicBool::new(true),
        });
        let changed_observed = Arc::new(AtomicUsize::new(0));
        let changed_obs_clone = Arc::clone(&changed_observed);

        let reg = app.world().resource::<ModRegistry>().clone();
        let dyn_sparse_rw = GuestSystem::new(
            "dyn_sparse_rw",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::read(pos_h), BamTermDesc::write(shield_h)],
            }],
            transfer_pos_x_to_sparse_shield,
            Arc::as_ptr(&state) as *mut c_void,
            reg,
        );

        let observe_changed =
            move |q: bevy_ecs::system::Query<&SparseShield, Changed<SparseShield>>| {
                changed_obs_clone.store(q.iter().count(), Ordering::SeqCst);
            };

        app.add_systems(Update, (dyn_sparse_rw, observe_changed).chain());

        // Frame 1: dynamic system mutates e1 & e2 -> Changed<SparseShield> sees 2
        app.update();
        assert_eq!(state.pass_counter.load(Ordering::SeqCst), 2);
        assert_eq!(changed_observed.load(Ordering::SeqCst), 2);
        assert_eq!(app.world().get::<SparseShield>(e1).unwrap().strength, 115.0);
        assert_eq!(app.world().get::<SparseShield>(e2).unwrap().strength, 225.0);
        assert!(app.world().get::<SparseShield>(e_unmatched).is_none());

        // Frame 2: only `rows_readonly` accessed -> Changed<SparseShield> must see 0!
        state.should_mutate.store(false, Ordering::SeqCst);
        app.update();
        assert_eq!(changed_observed.load(Ordering::SeqCst), 0);

        // Frame 3: re-enable mutation -> Changed<SparseShield> sees 2 again!
        state.should_mutate.store(true, Ordering::SeqCst);
        app.update();
        assert_eq!(changed_observed.load(Ordering::SeqCst), 2);
    }

    // ============================================================================
    // 3. `world.increment_change_tick()` Accuracy
    // ============================================================================

    unsafe extern "C" fn conditional_mutate_pos(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let should_mutate = unsafe { (*(user_data as *const AtomicBool)).load(Ordering::SeqCst) };
        if !should_mutate {
            return FfiResult::OK;
        }
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );
        let mut count = 0usize;
        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Position;
            for i in 0..count {
                unsafe { (*ptr.add(i)).x += 1.0 };
            }
        }
        FfiResult::OK
    }

    #[test]
    fn dynamic_system_advances_world_change_tick_correctly() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new())
            .unwrap();

        app.world_mut().spawn(Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });

        let do_mutate = Arc::new(AtomicBool::new(false));
        let seen_changed = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen_changed);

        let reg = app.world().resource::<ModRegistry>().clone();
        let dyn_sys = GuestSystem::new(
            "cond_mutate",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(pos_h)],
            }],
            conditional_mutate_pos,
            Arc::as_ptr(&do_mutate) as *mut c_void,
            reg,
        );

        let tick_before = app.world().read_change_tick();
        app.add_systems(
            Update,
            (
                dyn_sys,
                move |q: bevy_ecs::system::Query<&Position, Changed<Position>>| {
                    seen_clone.store(q.iter().count(), Ordering::SeqCst);
                },
            )
                .chain(),
        );

        // Frame 1: initial spawn observed, tick advances
        app.update();
        let tick_after_1 = app.world().read_change_tick();
        assert!(tick_after_1.get() > tick_before.get());
        assert_eq!(seen_changed.load(Ordering::SeqCst), 1);

        // Frame 2: do_mutate == false -> 0
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 0);

        // Frame 3: do_mutate == true -> 1
        do_mutate.store(true, Ordering::SeqCst);
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 1);

        // Frame 4: do_mutate == false -> 0
        do_mutate.store(false, Ordering::SeqCst);
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 0);
    }

    // ============================================================================
    // 4. Raw Byte Insertion & Droppable Component Soundness Guards
    // ============================================================================

    #[test]
    fn forbids_raw_byte_insertion_on_heap_owning_and_opaque_components() {
        const {
            assert!(!NameTag::IS_RAW_INSERTABLE);
            assert!(!FlaggedComp::IS_RAW_INSERTABLE);
            assert!(Velocity::IS_RAW_INSERTABLE);
        }

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let nametag_h = app
            .register_mod_component::<NameTag>(ComponentOptions::new())
            .unwrap();
        let flagged_h = app
            .register_mod_component::<FlaggedComp>(ComponentOptions::new())
            .unwrap();
        let vel_opaque_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new().opaque())
            .unwrap();

        let entity = app.world_mut().spawn_empty().id();
        let snap = app.world().resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, app.world().entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            let ffi_ent = FfiEntity::from_bits(entity.to_bits());

            let forged_name = vec![0u8; core::mem::size_of::<NameTag>()];
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_ent,
                        nametag_h,
                        forged_name.as_ptr(),
                        forged_name.len(),
                    )
                },
                FfiResult::PERMISSION_DENIED
            );

            let forged_bool = [2u8; core::mem::size_of::<FlaggedComp>()];
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_ent,
                        flagged_h,
                        forged_bool.as_ptr(),
                        forged_bool.len(),
                    )
                },
                FfiResult::PERMISSION_DENIED
            );

            let vel_bytes = [0u8; core::mem::size_of::<Velocity>()];
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        raw_cmds,
                        ffi_ent,
                        vel_opaque_h,
                        vel_bytes.as_ptr(),
                        vel_bytes.len(),
                    )
                },
                FfiResult::PERMISSION_DENIED
            );

            // Also verify `host_cmd_spawn` rejects all three with `PERMISSION_DENIED`
            let mut out_spawned = FfiEntity::NULL;
            for (h, ptr) in [
                (nametag_h, forged_name.as_ptr()),
                (flagged_h, forged_bool.as_ptr()),
                (vel_opaque_h, vel_bytes.as_ptr()),
            ] {
                let comps = [h];
                let ptrs = [ptr];
                assert_eq!(
                    unsafe {
                        host_cmd_spawn(raw_cmds, comps.as_ptr(), ptrs.as_ptr(), 1, &mut out_spawned)
                    },
                    FfiResult::PERMISSION_DENIED
                );
            }
        }

        storage.apply_to_world(app.world_mut(), &snap);
        assert!(app.world().get::<NameTag>(entity).is_none());
        assert!(app.world().get::<FlaggedComp>(entity).is_none());
        assert!(app.world().get::<Velocity>(entity).is_none());
    }

    #[test]
    fn droppable_dynamic_component_uses_clone_ignore_preventing_double_free() {
        static DROPPABLE_CLONE_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn droppable_drop_counter(_ptr: *mut c_void) {
            DROPPABLE_CLONE_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
        }

        DROPPABLE_CLONE_DROP_COUNT.store(0, Ordering::SeqCst);
        let mut world = World::new();
        let registry = ModRegistry::new();
        world.insert_resource(registry.clone());

        let is_active = Arc::new(AtomicBool::new(true));
        let mut load_ctx =
            LoadContext::with_world_and_mod_id(registry, is_active, &mut world as *mut World, None);

        let desc = ComponentDesc::new(
            c"sparse_test::ForeignOwnedHandle".as_ptr(),
            ComponentUuid::from_parts("sparse_test::ForeignOwnedHandle", 8, 8, 0),
            8,
            8,
        )
        .with_drop(droppable_drop_counter);

        let mut handle = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut handle) },
            FfiResult::OK
        );
        load_ctx.finish_init();

        let comp_id = world
            .resource::<ModRegistry>()
            .get_by_handle(handle)
            .unwrap()
            .component_id;

        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        let ffi_src = {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let raw_cmds = cmds.as_ffi_ptr();
            let mut ent = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_cmds, &mut ent) },
                FfiResult::OK
            );
            let payload = 0xDEAD_BEEF_CAFE_BABEu64.to_ne_bytes();
            assert_eq!(
                unsafe { host_cmd_insert(raw_cmds, ent, handle, payload.as_ptr(), payload.len()) },
                FfiResult::OK
            );
            ent
        };
        storage.apply_to_world(&mut world, &snap);

        let src_entity = Entity::try_from_bits(ffi_src.to_bits()).unwrap();
        assert!(world.entity(src_entity).contains_id(comp_id));

        // Clone entity via Bevy's EntityCloner: droppable dynamic component must be ignored!
        let cloned_entity = world.entity_mut(src_entity).clone_and_spawn();
        assert!(
            !world.entity(cloned_entity).contains_id(comp_id),
            "Droppable dynamic component must NOT be bit-copied onto cloned entity"
        );

        world.despawn(cloned_entity);
        world.despawn(src_entity);
        assert_eq!(
            DROPPABLE_CLONE_DROP_COUNT.load(Ordering::SeqCst),
            1,
            "Foreign destructor must run exactly once (no double-free from entity clone)"
        );
    }
}

mod optional_changed_resources_and_collisions {
    use bevy_abi_mod_host::{
        AbiModPlugin, ComponentCollisionError, ComponentOptions, ModAppExt, ModComponent,
        ModRegistry, ModSystemSet,
    };
    use bevy_abi_mod_host_sys::{
        LoadContext, host_cmd_insert, host_cmd_insert_default, host_cmd_remove,
        host_cmd_spawn_empty, host_get_proc_address, host_iter_column_mutable,
        host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_column_rows_readonly,
        host_iter_entities, host_iter_next_chunk, host_register_component, host_register_system,
        host_res_mut_ptr, host_res_ptr, host_sys_commands, host_sys_host_ctx, host_sys_query,
    };
    use bevy_abi_mod_sys::{
        BamCommands, BamComponent, BamIter, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc,
        ComponentDesc, ComponentUuid, FfiEntity, FfiResult, ModComponent as _, StrView,
        SymbolEntry, VTableHeader,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::{Added, Changed};
    use bevy_ecs::resource::Resource;
    use bevy_ecs::schedule::IntoScheduleConfigs;
    use bevy_ecs::system::Res;
    use bevy_ecs::world::World;
    use core::ffi::{CStr, c_void};
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "opt_res::Pos3")]
    pub struct Pos3 {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "opt_res::Vel3")]
    pub struct Vel3 {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "opt_res::Sensor")]
    pub struct Sensor {
        pub signal: u32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[component(storage = "SparseSet")]
    #[mod_component(name = "opt_res::SparseBoost")]
    pub struct SparseBoost {
        pub factor: f32,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "opt_res::RemovableMarker")]
    pub struct RemovableMarker {
        pub tag: u32,
    }

    #[derive(Component, ModComponent, Debug, Clone, PartialEq, Default)]
    #[mod_component(name = "opt_res::DefaultableTag")]
    pub struct DefaultableTag {
        pub label: String,
    }

    #[repr(C)]
    #[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
    #[mod_component(name = "opt_res::NoDefaultComp")]
    pub struct NoDefaultComp {
        pub code: u64,
    }

    #[repr(C)]
    #[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Default)]
    #[mod_component(name = "opt_res::SimConfig")]
    pub struct SimConfig {
        pub dt: f32,
        pub step_count: u32,
        pub banner_len_observed: u32,
    }

    #[derive(Resource, ModComponent, Debug, Clone, PartialEq, Default)]
    #[mod_component(name = "opt_res::WorldBanner")]
    pub struct WorldBanner {
        pub title: String,
    }

    fn register_inline_system(app: &mut App, desc: &BamSystemDesc) -> FfiResult {
        let registry = app.world().resource::<ModRegistry>().clone();
        let mut ctx = LoadContext::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            app.world_mut() as *mut World,
            None,
        );
        let status = unsafe { host_register_system(ctx.as_ffi_ptr(), desc) };
        ctx.finish_init();
        for sys in ctx.registered_systems.drain(..) {
            app.add_systems(Update, sys);
        }
        status
    }

    // ============================================================================
    // 1. OPTIONAL_READ & OPTIONAL_WRITE Across Dense Table + SparseSet Archetypes
    // ============================================================================

    #[test]
    fn test_optional_read_and_optional_write_across_table_and_sparse_archetypes() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Vel3>(ComponentOptions::new())
            .unwrap();
        let boost_h = app
            .register_mod_component::<SparseBoost>(ComponentOptions::new())
            .unwrap();

        let e_pos_vel = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: 10.0,
                    y: 0.0,
                    z: 0.0,
                },
                Vel3 {
                    x: 5.0,
                    y: 0.0,
                    z: 0.0,
                },
            ))
            .id();

        let e_pos_only = app
            .world_mut()
            .spawn(Pos3 {
                x: 100.0,
                y: 0.0,
                z: 0.0,
            })
            .id();

        let e_all = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: 1000.0,
                    y: 0.0,
                    z: 0.0,
                },
                Vel3 {
                    x: 2.0,
                    y: 0.0,
                    z: 0.0,
                },
                SparseBoost { factor: 3.0 },
            ))
            .id();

        let changed_boost_entities: Arc<Mutex<Vec<Entity>>> = Arc::new(Mutex::new(Vec::new()));

        unsafe extern "C" fn optional_kinematics_cb(
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
                // Column 1 is OPTIONAL_READ (`Vel3`): mutable borrow MUST be rejected!
                assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());

                let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
                let opt_vel_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const Vel3;
                // Column 2 is OPTIONAL_WRITE `SparseBoost` (`SparseSet` -> `rows_mutable`)
                let opt_boost_rows = unsafe { host_iter_column_rows_mutable(iter, 2) };
                assert!(!pos_ptr.is_null());

                for i in 0..count {
                    unsafe {
                        let vx = if !opt_vel_ptr.is_null() {
                            (*opt_vel_ptr.add(i)).x
                        } else {
                            1.0
                        };
                        let mult = if !opt_boost_rows.is_null() {
                            let b = &mut *((*opt_boost_rows.add(i)).cast::<SparseBoost>());
                            let old = b.factor;
                            b.factor += 1.0;
                            old
                        } else {
                            1.0
                        };
                        (*pos_ptr.add(i)).x += vx * mult;
                    }
                }
            }
            FfiResult::OK
        }

        // Prime initial spawn ticks before registering guest system
        app.update();

        let terms = [
            BamTermDesc::write(pos_h),
            BamTermDesc::optional_read(vel_h),
            BamTermDesc::optional_write(boost_h),
        ];
        let query = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let desc = BamSystemDesc::new(
            c"optional_kinematics".as_ptr(),
            &query,
            1,
            optional_kinematics_cb,
            core::ptr::null_mut(),
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        let boost_log = Arc::clone(&changed_boost_entities);
        app.add_systems(
            Update,
            (move |q: bevy_ecs::system::Query<Entity, Changed<SparseBoost>>| {
                let mut guard = boost_log.lock().unwrap();
                guard.clear();
                for e in &q {
                    guard.push(e);
                }
            })
            .after(ModSystemSet::All),
        );

        app.update();

        let world = app.world();
        assert_eq!(world.get::<Pos3>(e_pos_vel).unwrap().x, 15.0);
        assert_eq!(world.get::<Pos3>(e_pos_only).unwrap().x, 101.0);
        assert_eq!(world.get::<Pos3>(e_all).unwrap().x, 1006.0);
        assert_eq!(world.get::<SparseBoost>(e_all).unwrap().factor, 4.0);
        assert_eq!(*changed_boost_entities.lock().unwrap(), vec![e_all]);
    }

    // ============================================================================
    // 2. CHANGED & ADDED Row-Level Filtering, Chunk Coalescing & Differential Parity
    // ============================================================================

    #[derive(Default)]
    struct TickFilterLog {
        changed_entities: Vec<u64>,
        chunk_lengths: Vec<usize>,
        added_entities: Vec<u64>,
    }

    #[test]
    fn test_changed_and_added_row_level_coalescing_and_differential_bevy_parity() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        let _vel_h = app
            .register_mod_component::<Vel3>(ComponentOptions::new())
            .unwrap();
        let sensor_h = app
            .register_mod_component::<Sensor>(ComponentOptions::new())
            .unwrap();
        let _boost_h = app
            .register_mod_component::<SparseBoost>(ComponentOptions::new())
            .unwrap();

        let mut entities = Vec::new();
        for i in 0..12u32 {
            let e = app
                .world_mut()
                .spawn((
                    Pos3 {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Sensor { signal: i },
                ))
                .id();
            entities.push(e);
        }

        let c_abi_log = Arc::new(Mutex::new(TickFilterLog::default()));
        let native_added_log: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));

        unsafe extern "C" fn changed_pos_cb(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
            let mut guard = log_mutex.lock().unwrap();
            let mut iter: *mut BamIter = core::ptr::null_mut();
            assert_eq!(
                unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
                FfiResult::OK
            );
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
                guard.chunk_lengths.push(count);
                // Column 0 is WRITE Pos3, Column 1 is CHANGED Pos3 (suppressed column data when paired with WRITE!)
                assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
                let ents = unsafe { host_iter_entities(iter) };
                let pos_mut = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
                assert!(!ents.is_null() && !pos_mut.is_null());
                for row in 0..count {
                    unsafe {
                        guard.changed_entities.push((*ents.add(row)).0);
                        (*pos_mut.add(row)).y += 1.0;
                    }
                }
            }
            FfiResult::OK
        }

        unsafe extern "C" fn added_sensor_cb(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
            let mut guard = log_mutex.lock().unwrap();
            let mut iter: *mut BamIter = core::ptr::null_mut();
            assert_eq!(
                unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
                FfiResult::OK
            );
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
                let ents = unsafe { host_iter_entities(iter) };
                let sensor_ro = unsafe { host_iter_column_readonly(iter, 0) } as *const Sensor;
                assert!(!ents.is_null() && !sensor_ro.is_null());
                for row in 0..count {
                    unsafe {
                        let _sig = (*sensor_ro.add(row)).signal;
                        guard.added_entities.push((*ents.add(row)).0);
                    }
                }
            }
            FfiResult::OK
        }

        let log_ptr = Arc::as_ptr(&c_abi_log) as *mut c_void;

        let changed_terms = [BamTermDesc::write(pos_h), BamTermDesc::changed(pos_h)];
        let changed_q = BamQueryDesc::new(changed_terms.as_ptr(), changed_terms.len());
        let changed_desc = BamSystemDesc::new(
            c"changed_pos_sys".as_ptr(),
            &changed_q,
            1,
            changed_pos_cb,
            log_ptr,
        );
        assert_eq!(
            register_inline_system(&mut app, &changed_desc),
            FfiResult::OK
        );

        let added_terms = [BamTermDesc::added(sensor_h)];
        let added_q = BamQueryDesc::new(added_terms.as_ptr(), added_terms.len());
        let added_desc = BamSystemDesc::new(
            c"added_sensor_sys".as_ptr(),
            &added_q,
            1,
            added_sensor_cb,
            log_ptr,
        );
        assert_eq!(register_inline_system(&mut app, &added_desc), FfiResult::OK);

        let native_added_ref = Arc::clone(&native_added_log);
        app.add_systems(
            Update,
            move |q_added: bevy_ecs::system::Query<Entity, Added<Sensor>>| {
                let mut guard = native_added_ref.lock().unwrap();
                for e in &q_added {
                    guard.insert(e.to_bits());
                }
            },
        );

        // Frame 1: All 12 initial entities match BOTH `Changed<Pos3>` (single 12-row chunk) AND `Added<Sensor>`
        app.update();
        {
            let mut guard = c_abi_log.lock().unwrap();
            let c_changed: BTreeSet<u64> = guard.changed_entities.drain(..).collect();
            let c_added: BTreeSet<u64> = guard.added_entities.drain(..).collect();
            let chunks = std::mem::take(&mut guard.chunk_lengths);
            let expected_all: BTreeSet<u64> = entities.iter().map(|e| e.to_bits()).collect();
            assert_eq!(c_changed, expected_all);
            assert_eq!(chunks, vec![12]);
            let native_added = std::mem::take(&mut *native_added_log.lock().unwrap());
            assert_eq!(c_added, native_added);
            assert_eq!(c_added.len(), 12);
        }

        // Frame 2: System must NOT observe its own prior-frame mutation as `Changed<Pos3>`!
        app.update();
        {
            let guard = c_abi_log.lock().unwrap();
            assert!(guard.changed_entities.is_empty());
            assert!(guard.chunk_lengths.is_empty());
            assert!(guard.added_entities.is_empty());
            assert!(native_added_log.lock().unwrap().is_empty());
        }

        // Frame 3: Mutate non-contiguous row runs `[1, 2, 3]`, `[7]`, `[10, 11]` + spawn 2 new entities
        let mutated_indices = [1usize, 2, 3, 7, 10, 11];
        for &idx in &mutated_indices {
            app.world_mut().get_mut::<Pos3>(entities[idx]).unwrap().x += 50.0;
        }
        app.world_mut()
            .get_mut::<Sensor>(entities[5])
            .unwrap()
            .signal = 999;

        let e_new_1 = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: 500.0,
                    y: 0.0,
                    z: 0.0,
                },
                Sensor { signal: 100 },
                Vel3::default(),
            ))
            .id();
        let e_new_2 = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: 600.0,
                    y: 0.0,
                    z: 0.0,
                },
                Sensor { signal: 101 },
                Vel3::default(),
            ))
            .id();

        app.update();
        {
            let mut guard = c_abi_log.lock().unwrap();
            let c_changed: BTreeSet<u64> = guard.changed_entities.drain(..).collect();
            let c_added: BTreeSet<u64> = guard.added_entities.drain(..).collect();
            let chunks = std::mem::take(&mut guard.chunk_lengths);

            let mut expected_changed: BTreeSet<u64> = mutated_indices
                .iter()
                .map(|&i| entities[i].to_bits())
                .collect();
            expected_changed.insert(e_new_1.to_bits());
            expected_changed.insert(e_new_2.to_bits());

            assert_eq!(c_changed, expected_changed);
            assert_eq!(chunks, vec![3, 1, 2, 2]);

            let native_added = std::mem::take(&mut *native_added_log.lock().unwrap());
            let expected_added: BTreeSet<u64> =
                [e_new_1.to_bits(), e_new_2.to_bits()].into_iter().collect();
            assert_eq!(c_added, expected_added);
            assert_eq!(c_added, native_added);
        }
    }

    #[test]
    fn test_sparse_set_changed_and_added_filters() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let boost_h = app
            .register_mod_component::<SparseBoost>(ComponentOptions::new())
            .unwrap();

        let e1 = app.world_mut().spawn(SparseBoost { factor: 1.0 }).id();
        let e2 = app.world_mut().spawn(SparseBoost { factor: 2.0 }).id();
        let e3 = app.world_mut().spawn(SparseBoost { factor: 3.0 }).id();

        let seen_changed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        unsafe extern "C" fn sparse_changed_cb(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let vec_mutex = unsafe { &*(user_data as *const Mutex<Vec<u64>>) };
            let mut guard = vec_mutex.lock().unwrap();
            let mut iter: *mut BamIter = core::ptr::null_mut();
            assert_eq!(
                unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
                FfiResult::OK
            );
            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
                let ents = unsafe { host_iter_entities(iter) };
                let boost_rows = unsafe { host_iter_column_rows_readonly(iter, 0) };
                assert!(!ents.is_null() && !boost_rows.is_null());
                for r in 0..count {
                    unsafe {
                        let _val = (*(*boost_rows.add(r)).cast::<SparseBoost>()).factor;
                        guard.push((*ents.add(r)).0);
                    }
                }
            }
            FfiResult::OK
        }

        let terms = [BamTermDesc::changed(boost_h)];
        let query = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let desc = BamSystemDesc::new(
            c"sparse_changed_sys".as_ptr(),
            &query,
            1,
            sparse_changed_cb,
            Arc::as_ptr(&seen_changed) as *mut c_void,
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        // Frame 1: all 3 seen
        app.update();
        assert_eq!(seen_changed.lock().unwrap().drain(..).count(), 3);

        // Frame 2: none changed (summary tick check + row tick check)
        app.update();
        assert!(seen_changed.lock().unwrap().is_empty());

        // Frame 3: mutate only `e2`!
        app.world_mut().get_mut::<SparseBoost>(e2).unwrap().factor = 99.0;
        let _ = (e1, e3);
        app.update();
        assert_eq!(
            seen_changed.lock().unwrap().drain(..).collect::<Vec<_>>(),
            vec![e2.to_bits()]
        );
    }

    // ============================================================================
    // 3. ECS Resources (POD + Opaque VTable, Change Ticks, & Permissions)
    // ============================================================================

    #[repr(C)]
    struct WorldBannerVTableMirror {
        header: VTableHeader,
        get_title: Option<unsafe extern "C" fn(*const c_void) -> StrView>,
        set_title: Option<unsafe extern "C" fn(*mut c_void, StrView) -> FfiResult>,
    }

    struct ResourceTestCtx {
        sim_cfg_comp: BamComponent,
        write_trigger: AtomicU8,
    }

    #[test]
    fn test_ecs_resources_pod_and_opaque_vtable_and_change_ticks() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let sim_cfg_h = app.register_mod_resource::<SimConfig>().unwrap();
        let banner_h = app.register_mod_resource::<WorldBanner>().unwrap();
        app.insert_resource(SimConfig {
            dt: 0.5,
            step_count: 0,
            banner_len_observed: 0,
        });
        app.insert_resource(WorldBanner {
            title: "BevyAbiMod".to_string(),
        });

        app.update();

        unsafe extern "C" fn pure_resource_sys(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let ctx = unsafe { &*(user_data as *const ResourceTestCtx) };
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

            // Out-of-bounds term index MUST return null!
            assert!(unsafe { host_res_ptr(iter, 99) }.is_null());

            // Term 1 (`WorldBanner`) was declared `RESOURCE_READ`: mutable borrow MUST return null!
            assert!(unsafe { host_res_mut_ptr(iter, 1) }.is_null());

            // Attempting `cmd_insert` on a Resource handle MUST be rejected with `INVALID_COMPONENT`!
            let dummy = SimConfig::default();
            let bad_cmd = unsafe {
                host_cmd_insert(
                    commands,
                    FfiEntity::from_bits(1),
                    ctx.sim_cfg_comp,
                    (&dummy as *const SimConfig).cast::<u8>(),
                    core::mem::size_of::<SimConfig>(),
                )
            };
            assert_eq!(bad_cmd, FfiResult::INVALID_COMPONENT);

            // Read `WorldBanner` via its generated `"bam.v0.*"` VTable!
            let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
            let banner_vt_ptr = unsafe {
                host_get_proc_address(host_ctx, c"bam.v0.opt_res::WorldBanner.vtable".as_ptr())
            } as *const WorldBannerVTableMirror;
            assert!(!banner_vt_ptr.is_null());
            assert_eq!(unsafe { (*banner_vt_ptr).header.uuid }, WorldBanner::UUID);
            let banner_ptr = unsafe { host_res_ptr(iter, 1) };
            assert!(!banner_ptr.is_null());
            let view =
                unsafe { ((*banner_vt_ptr).get_title.unwrap())(banner_ptr.cast::<c_void>()) };

            if ctx.write_trigger.load(Ordering::SeqCst) != 0 {
                // Mutate `SimConfig` (term 0) -> marks Bevy resource change tick!
                let cfg_ptr = unsafe { host_res_mut_ptr(iter, 0) } as *mut SimConfig;
                assert!(!cfg_ptr.is_null());
                unsafe {
                    (*cfg_ptr).step_count += 1;
                    (*cfg_ptr).banner_len_observed = view.len as u32;
                }
            } else {
                // Read-only access even when `RESOURCE_WRITE` is declared -> MUST NOT mark changed!
                let cfg_ro = unsafe { host_res_ptr(iter, 0) } as *const SimConfig;
                assert!(!cfg_ro.is_null());
            }

            // Pure resource-only query has 0 entity chunks!
            let mut count = 99usize;
            assert!(!unsafe { host_iter_next_chunk(iter, &mut count) }.get());
            assert_eq!(count, 0);
            FfiResult::OK
        }

        let res_ctx = Arc::new(ResourceTestCtx {
            sim_cfg_comp: sim_cfg_h,
            write_trigger: AtomicU8::new(1),
        });

        let terms = [
            BamTermDesc::resource_write(sim_cfg_h),
            BamTermDesc::resource_read(banner_h),
        ];
        let query = BamQueryDesc::new(terms.as_ptr(), terms.len());
        let desc = BamSystemDesc::new(
            c"pure_resource_sys".as_ptr(),
            &query,
            1,
            pure_resource_sys,
            Arc::as_ptr(&res_ctx) as *mut c_void,
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        // Frame 1: mutates `SimConfig`
        app.update();
        assert_eq!(
            *app.world().resource::<SimConfig>(),
            SimConfig {
                dt: 0.5,
                step_count: 1,
                banner_len_observed: "BevyAbiMod".len() as u32,
            }
        );

        let native_saw_changed = Arc::new(Mutex::new(false));
        let saw_clone = Arc::clone(&native_saw_changed);
        app.add_systems(
            Update,
            (move |res: Res<SimConfig>| {
                use bevy_ecs::change_detection::DetectChanges;
                *saw_clone.lock().unwrap() = res.is_changed();
            })
            .after(ModSystemSet::All),
        );

        // Frame 2: `write_trigger = 1` -> `is_changed()` MUST be `true`!
        app.update();
        assert!(*native_saw_changed.lock().unwrap());
        assert_eq!(app.world().resource::<SimConfig>().step_count, 2);

        // Frame 3: `write_trigger = 0` (calls only `host_res_ptr`) -> `is_changed()` MUST be `false`!
        res_ctx.write_trigger.store(0, Ordering::SeqCst);
        app.update();
        assert!(
            !*native_saw_changed.lock().unwrap(),
            "Calling only `host_res_ptr` must not mark Bevy Resource as changed!"
        );
        assert_eq!(app.world().resource::<SimConfig>().step_count, 2);
    }

    // ============================================================================
    // 4. Deferred `host_cmd_remove` & `host_cmd_insert_default` (Opaque String)
    // ============================================================================

    #[repr(C)]
    struct DefaultableTagVTableMirror {
        header: VTableHeader,
        get_label: Option<unsafe extern "C" fn(*const c_void) -> StrView>,
        set_label: Option<unsafe extern "C" fn(*mut c_void, StrView) -> FfiResult>,
    }

    #[test]
    fn test_cmd_remove_component_and_cmd_insert_default_opaque_string_workflow() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let _pos_h = app
            .register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        let removable_h = app
            .register_mod_component::<RemovableMarker>(ComponentOptions::new())
            .unwrap();
        let no_def_h = app
            .register_mod_component::<NoDefaultComp>(ComponentOptions::new())
            .unwrap();
        let def_tag_h = app
            .register_mod_component::<DefaultableTag>(ComponentOptions::new().with_default())
            .unwrap();

        let existing = app
            .world_mut()
            .spawn((
                Pos3 {
                    x: 7.0,
                    y: 0.0,
                    z: 0.0,
                },
                RemovableMarker { tag: 123 },
            ))
            .id();

        struct CmdTestCtx {
            existing_entity: FfiEntity,
            removable_h: BamComponent,
            no_def_h: BamComponent,
            def_tag_h: BamComponent,
            spawned_entity: AtomicUsize,
            did_spawn: AtomicU8,
        }

        let ctx = Box::new(CmdTestCtx {
            existing_entity: FfiEntity::from_bits(existing.to_bits()),
            removable_h,
            no_def_h,
            def_tag_h,
            spawned_entity: AtomicUsize::new(0),
            did_spawn: AtomicU8::new(0),
        });

        unsafe extern "C" fn spawner_and_remover_sys(
            sys_ctx: *mut BamSystemCtx,
            user_data: *mut c_void,
        ) -> FfiResult {
            let ctx = unsafe { &*(user_data as *const CmdTestCtx) };
            if ctx.did_spawn.swap(1, Ordering::SeqCst) != 0 {
                return FfiResult::OK;
            }
            let mut commands: *mut BamCommands = core::ptr::null_mut();
            assert_eq!(
                unsafe { host_sys_commands(sys_ctx, &mut commands) },
                FfiResult::OK
            );

            // 1. Remove `RemovableMarker` from `existing_entity`
            assert_eq!(
                unsafe { host_cmd_remove(commands, ctx.existing_entity, ctx.removable_h) },
                FfiResult::OK
            );

            // 2. Spawn empty entity and attach `DefaultableTag` via `host_cmd_insert_default`
            let mut spawned = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(commands, &mut spawned) },
                FfiResult::OK
            );
            assert!(!spawned.is_null());

            // Raw byte insertion on `DefaultableTag` (`String`) MUST return `PERMISSION_DENIED`!
            let fake_bytes = [0u8; core::mem::size_of::<DefaultableTag>()];
            assert_eq!(
                unsafe {
                    host_cmd_insert(
                        commands,
                        spawned,
                        ctx.def_tag_h,
                        fake_bytes.as_ptr(),
                        fake_bytes.len(),
                    )
                },
                FfiResult::PERMISSION_DENIED
            );

            // `host_cmd_insert_default` on `NoDefaultComp` (registered without `Default`) MUST return `PERMISSION_DENIED`!
            assert_eq!(
                unsafe { host_cmd_insert_default(commands, spawned, ctx.no_def_h) },
                FfiResult::PERMISSION_DENIED
            );

            // `host_cmd_insert_default` on `DefaultableTag` MUST succeed!
            assert_eq!(
                unsafe { host_cmd_insert_default(commands, spawned, ctx.def_tag_h) },
                FfiResult::OK
            );

            ctx.spawned_entity
                .store(spawned.0 as usize, Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn tag_vtable_mutator_sys(
            sys_ctx: *mut BamSystemCtx,
            _user_data: *mut c_void,
        ) -> FfiResult {
            let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
            let vt_ptr = unsafe {
                host_get_proc_address(host_ctx, c"bam.v0.opt_res::DefaultableTag.vtable".as_ptr())
            } as *const DefaultableTagVTableMirror;
            assert!(!vt_ptr.is_null());
            let mut iter: *mut BamIter = core::ptr::null_mut();
            assert_eq!(
                unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
                FfiResult::OK
            );

            let mut count = 0usize;
            while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
                let base = unsafe { host_iter_column_mutable(iter, 0) };
                assert!(!base.is_null());
                let view = StrView::from_str("created_via_default_and_vtable");
                for r in 0..count {
                    unsafe {
                        let stride = (*vt_ptr).header.size;
                        let elem = base.add(r * stride).cast::<c_void>();
                        let rc = ((*vt_ptr).set_label.unwrap())(elem, view);
                        assert_eq!(rc, FfiResult::OK);
                    }
                }
            }
            FfiResult::OK
        }

        let spawn_desc = BamSystemDesc::new(
            c"spawner_and_remover_sys".as_ptr(),
            core::ptr::null(),
            0,
            spawner_and_remover_sys,
            (&*ctx as *const CmdTestCtx) as *mut c_void,
        );
        assert_eq!(register_inline_system(&mut app, &spawn_desc), FfiResult::OK);

        let tag_terms = [BamTermDesc::write(def_tag_h)];
        let tag_q = BamQueryDesc::new(tag_terms.as_ptr(), tag_terms.len());
        let mut_desc = BamSystemDesc::new(
            c"tag_vtable_mutator_sys".as_ptr(),
            &tag_q,
            1,
            tag_vtable_mutator_sys,
            core::ptr::null_mut(),
        );
        assert_eq!(register_inline_system(&mut app, &mut_desc), FfiResult::OK);

        // Frame 1: Queues `remove` + `spawn_empty` + `insert_default`
        app.update();
        assert!(app.world().get::<RemovableMarker>(existing).is_none());
        assert_eq!(app.world().get::<Pos3>(existing).unwrap().x, 7.0);

        let spawned_bits = ctx.spawned_entity.load(Ordering::SeqCst) as u64;
        let spawned_entity = Entity::from_bits(spawned_bits);
        assert!(app.world().get::<DefaultableTag>(spawned_entity).is_some());

        // Frame 2: `tag_vtable_mutator_sys` mutates the default-inserted `DefaultableTag` via VTable!
        app.update();
        assert_eq!(
            app.world()
                .get::<DefaultableTag>(spawned_entity)
                .unwrap()
                .label,
            "created_via_default_and_vtable"
        );

        drop(ctx);
    }

    // ============================================================================
    // 5. Component UUID & Metadata Collision Detection
    // ============================================================================

    #[repr(C)]
    #[derive(Component)]
    struct SameNameDifferentSize {
        a: f64,
        b: f64,
        c: f64,
    }

    impl bevy_abi_mod_sys::ModComponent for SameNameDifferentSize {
        const CANONICAL_NAME: &'static str = "opt_res::Pos3";
        const COMPONENT_CSTR: &'static CStr = c"opt_res::Pos3";
        const UUID: ComponentUuid = Pos3::UUID;
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader =
                VTableHeader::new::<VTableHeader, SameNameDifferentSize>(Pos3::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    #[repr(C)]
    #[derive(Component)]
    struct DifferentNameSameUuid {
        x: f32,
        y: f32,
        z: f32,
    }

    impl bevy_abi_mod_sys::ModComponent for DifferentNameSameUuid {
        const CANONICAL_NAME: &'static str = "opt_res::ForgedPos3Alias";
        const COMPONENT_CSTR: &'static CStr = c"opt_res::ForgedPos3Alias";
        const UUID: ComponentUuid = Pos3::UUID;
        const IS_ABI_POD: bool = true;
        const IS_RAW_INSERTABLE: bool = true;
        type VTable = VTableHeader;
        fn vtable() -> &'static Self::VTable {
            static VT: VTableHeader =
                VTableHeader::new::<VTableHeader, DifferentNameSameUuid>(Pos3::UUID);
            &VT
        }
        fn symbols() -> &'static [SymbolEntry] {
            &[]
        }
    }

    #[test]
    fn test_component_uuid_and_metadata_collision_detection() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        // 1. Initial registration + idempotent re-registration return the same 1-based handle
        let h1 = app
            .register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        let h2 = app
            .register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        assert_eq!(h1, h2);

        // 2. Same name with mismatched size/align -> MetadataMismatch
        let err_meta = app
            .register_mod_component::<SameNameDifferentSize>(ComponentOptions::new())
            .expect_err("Mismatched size/align under existing name must fail!");
        assert!(matches!(
            err_meta,
            ComponentCollisionError::MetadataMismatch { .. }
        ));

        // 3. Different name declaring an existing UUID -> UuidCollision
        let err_uuid = app
            .register_mod_component::<DifferentNameSameUuid>(ComponentOptions::new())
            .expect_err("Duplicate ComponentUuid under distinct name must fail!");
        assert!(matches!(
            err_uuid,
            ComponentCollisionError::UuidCollision { .. }
        ));

        // 4. Re-registering an entity component as a Resource -> MetadataMismatch
        #[repr(C)]
        #[derive(Resource)]
        struct Pos3ResourceCollision {
            x: f32,
            y: f32,
            z: f32,
        }
        impl bevy_abi_mod_sys::ModComponent for Pos3ResourceCollision {
            const CANONICAL_NAME: &'static str = "opt_res::Pos3";
            const COMPONENT_CSTR: &'static CStr = c"opt_res::Pos3";
            const UUID: ComponentUuid = Pos3::UUID;
            const IS_ABI_POD: bool = true;
            const IS_RAW_INSERTABLE: bool = true;
            type VTable = VTableHeader;
            fn vtable() -> &'static Self::VTable {
                static VT: VTableHeader =
                    VTableHeader::new::<VTableHeader, Pos3ResourceCollision>(Pos3::UUID);
                &VT
            }
            fn symbols() -> &'static [SymbolEntry] {
                &[]
            }
        }
        let err_res_mismatch = app
            .register_mod_resource::<Pos3ResourceCollision>()
            .expect_err("Registering an entity component name as a Resource must fail!");
        assert!(matches!(
            err_res_mismatch,
            ComponentCollisionError::MetadataMismatch { .. }
        ));

        // 5. Dynamic `host_register_component` C-ABI collision guard (`TYPE_MISMATCH`)
        let registry = app.world().resource::<ModRegistry>().clone();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            app.world_mut() as *mut World,
            None,
        );

        let forged_desc =
            ComponentDesc::new(c"opt_res::DynamicForgedUuid".as_ptr(), Pos3::UUID, 8, 4);
        let mut out = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &forged_desc, &mut out) },
            FfiResult::TYPE_MISMATCH
        );

        let valid_dyn = ComponentDesc::new(
            c"opt_res::ValidDynComp".as_ptr(),
            ComponentUuid::from_parts("opt_res::ValidDynComp", 8, 4, 0),
            8,
            4,
        );
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &valid_dyn, &mut out) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &valid_dyn, &mut out) },
            FfiResult::OK
        );

        let mismatched_dyn = ComponentDesc {
            size: 16,
            ..valid_dyn
        };
        assert_eq!(
            unsafe { host_register_component(load_ctx.as_ffi_ptr(), &mismatched_dyn, &mut out) },
            FfiResult::TYPE_MISMATCH
        );
        load_ctx.finish_init();
    }

    #[test]
    #[should_panic(expected = "collision")]
    fn test_conflicting_static_register_mod_component_panics_with_clear_message() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Pos3>(ComponentOptions::new())
            .unwrap();
        if let Err(e) = app.register_mod_component::<DifferentNameSameUuid>(ComponentOptions::new())
        {
            panic!("{e}");
        }
    }
}
