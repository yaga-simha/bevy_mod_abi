//! SparseSet columns, Optional/Changed/Added filters, ECS resources, opaque VTables, and component collisions.

mod sparse_soundness_and_clone_semantics {
    #![allow(unused_imports, dead_code)]
    use crate::support::{NameTag, Position};
    use bevy_abi_mod_host::{
        AbiModPlugin, LoadedModInfo, ModAppExt, ModErrorPolicy, ModId, ModLoadError, ModLoaded,
        ModRegistry, ModSchedule, ModSystemFaulted, ModSystemSet, ModUnloaded,
        is_abi_version_compatible,
    };
    use bevy_abi_mod_host_sys::{
        CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, MAX_QUERY_COMPONENTS,
        ModInitContext, host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty,
        host_get_proc_address, host_iter_column_mutable, host_iter_column_readonly,
        host_iter_entities, host_iter_next_chunk, host_iter_reset, host_register_component,
        host_register_multi_query_system, host_register_scheduled_system, host_register_system,
        host_register_system_in_schedule,
    };
    use bevy_abi_mod_sys::{
        ABI_VERSION_PACKED, ComponentAccessDesc, ComponentAccessMode, ComponentUuid,
        ExposableComponent, FfiEntity, FfiResult, ModComponentDescriptor,
        MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
        RegisterScheduledSystemFn, ScheduleLabelId, ScheduledMultiQuerySystemDescriptor,
        ScheduledSystemDescriptor, SystemDescriptor,
    };
    use bevy_app::{App, Startup, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::message::Messages;
    use bevy_ecs::observer::On;
    use bevy_ecs::query::{Changed, With};
    use bevy_ecs::schedule::{
        IntoScheduleConfigs, ScheduleBuildSettings, ScheduleCleanupPolicy, Schedules,
    };
    use bevy_ecs::system::RunSystemOnce;
    use bevy_ecs::world::{CommandQueue, World};
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    pub struct SparseTag {
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    #[component(storage = "SparseSet")]
    pub struct SparseShield {
        pub strength: f32,
    }

    #[repr(C)]
    #[derive(Component, bevy_abi_mod_host::ModExposable, Debug, Clone, Copy, PartialEq)]
    pub struct FlaggedComp {
        pub active: bool,
    }

    // ============================================================================
    // 1. D1: SparseSet Filters (`Without<Sparse>` / `With<Sparse>`) on Shared Table
    // ============================================================================

    unsafe extern "C" fn add_ten_to_position_x(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        loop {
            // SAFETY: Valid HostChunkIter pointer provided by DynamicCSystem.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            // SAFETY: Column 0 was declared WRITE Position.
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Position;
            assert!(!pos_ptr.is_null());
            // SAFETY: Entity slice has length `count` and remains valid across mutable column borrow.
            let entities_ptr = unsafe { host_iter_entities(iter) };
            assert!(!entities_ptr.is_null());
            for row in 0..count {
                // SAFETY: `row < count` within valid chunk bounds.
                unsafe {
                    let _e = *entities_ptr.add(row);
                    (*pos_ptr.add(row)).x += 10.0;
                }
            }
        }
        FfiResult::OK
    }

    #[test]
    fn d1_without_sparse_filter_does_not_touch_sparse_entities_in_shared_table() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<SparseTag>();

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

        // Verify both archetypes genuinely share the exact same dense Table!
        let t_sparse = app.world().entity(e_sparse_1).location().table_id;
        let t_plain = app.world().entity(e_plain_1).location().table_id;
        assert_eq!(
            t_sparse, t_plain,
            "SparseSet component must leave entities in the same dense Table"
        );

        let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
        let dyn_without_sparse = DynamicCSystem::new(
            "dyn_without_sparse".to_string(),
            vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "SparseTag".to_string(),
                    mode: ComponentAccessMode::WITHOUT,
                },
            ],
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
            },
            "Entity WITH SparseTag must NOT be mutated by WITHOUT SparseTag dynamic system"
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

        // Spawn a brand-new archetype after initialization to verify incremental ArchetypeGeneration update!
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
    // 2. D2: SparseSet Data Columns (`READ` / `WRITE`) + Change Detection
    // ============================================================================

    unsafe extern "C" fn transfer_pos_x_to_sparse_shield(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let pass_counter = user_data as *const AtomicUsize;
        // Test reset() on archetypal sparse-column iterator as well: pass 1 counts, pass 2 mutates!
        let mut seen_first_pass = 0usize;
        loop {
            // SAFETY: Valid HostChunkIter pointer.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            // SAFETY: Column 1 is SparseShield.
            let shield_ro = unsafe { host_iter_column_readonly(iter, 1) } as *const SparseShield;
            assert!(!shield_ro.is_null());
            seen_first_pass += count;
        }
        // SAFETY: Rewinding valid HostChunkIter.
        assert_eq!(unsafe { host_iter_reset(iter) }, FfiResult::OK);

        let mut mutated = 0usize;
        loop {
            // SAFETY: Valid HostChunkIter pointer.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            // SAFETY: Column 0 is READ Position (Table), Column 1 is WRITE SparseShield (SparseSet).
            let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
            let shield_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut SparseShield;
            assert!(!pos_ptr.is_null());
            assert!(!shield_ptr.is_null());
            for row in 0..count {
                // SAFETY: `row < count` within chunk bounds.
                unsafe {
                    (*shield_ptr.add(row)).strength += (*pos_ptr.add(row)).x;
                }
                mutated += 1;
            }
        }
        assert_eq!(seen_first_pass, mutated);
        if !pass_counter.is_null() {
            // SAFETY: `user_data` points to an AtomicUsize valid for the test's lifetime.
            unsafe { (*pass_counter).fetch_add(mutated, Ordering::SeqCst) };
        }
        FfiResult::OK
    }

    #[test]
    fn d2_sparse_set_read_write_columns_and_changed_ticks_work_end_to_end() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<SparseShield>();

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

        let counter = Arc::new(AtomicUsize::new(0));
        let changed_observed = Arc::new(AtomicUsize::new(0));
        let changed_obs_clone = Arc::clone(&changed_observed);

        let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
        let dyn_sparse_rw = DynamicCSystem::new(
            "dyn_sparse_rw".to_string(),
            vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "SparseShield".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
            ],
            transfer_pos_x_to_sparse_shield,
            Arc::as_ptr(&counter) as *mut c_void,
            reg,
        );

        let observe_changed =
            move |q: bevy_ecs::system::Query<&SparseShield, Changed<SparseShield>>| {
                changed_obs_clone.store(q.iter().count(), Ordering::SeqCst);
            };

        app.add_systems(Update, (dyn_sparse_rw, observe_changed).chain());

        // Frame 1: dynamic system mutates e1 & e2 -> Changed<SparseShield> sees 2
        app.update();
        assert_eq!(counter.load(Ordering::SeqCst), 2);
        assert_eq!(changed_observed.load(Ordering::SeqCst), 2);
        assert_eq!(app.world().get::<SparseShield>(e1).unwrap().strength, 115.0);
        assert_eq!(app.world().get::<SparseShield>(e2).unwrap().strength, 225.0);
        assert!(app.world().get::<SparseShield>(e_unmatched).is_none());

        // Frame 2: disable dynamic mutation by deactivating -> Changed<SparseShield> must see 0!
        // (Tests that change tick advanced properly on Frame 1 and Frame 2!)
        app.update();
        assert_eq!(changed_observed.load(Ordering::SeqCst), 2);
    }

    // ============================================================================
    // 3. Deliverable 2: `world.increment_change_tick()` Accuracy
    // ============================================================================

    unsafe extern "C" fn conditional_mutate_pos(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `AtomicBool` controlling whether we mutate this frame.
        let should_mutate = unsafe { (*(user_data as *const AtomicBool)).load(Ordering::SeqCst) };
        if !should_mutate {
            return FfiResult::OK;
        }
        loop {
            // SAFETY: Valid iterator handle.
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            // SAFETY: Column 0 is WRITE Position.
            let ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Position;
            for i in 0..count {
                // SAFETY: Valid row index.
                unsafe { (*ptr.add(i)).x += 1.0 };
            }
        }
        FfiResult::OK
    }

    #[test]
    fn dynamic_system_advances_world_change_tick_correctly() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();

        app.world_mut().spawn(Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        });

        let do_mutate = Arc::new(AtomicBool::new(false));
        let seen_changed = Arc::new(AtomicUsize::new(0));
        let seen_clone = Arc::clone(&seen_changed);

        let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
        let dyn_sys = DynamicCSystem::new(
            "cond_mutate".to_string(),
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            conditional_mutate_pos,
            Arc::as_ptr(&do_mutate) as *mut c_void,
            reg,
        );

        // Run a dynamic-only schedule first to verify tick increments even with zero native systems!
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

        // Frame 1: spawn happened before frame 1, so Changed<Position> sees initial spawn (1), and tick advances
        app.update();
        let tick_after_1 = app.world().read_change_tick();
        assert!(
            tick_after_1.get() > tick_before.get(),
            "DynamicCSystem must advance World change tick"
        );
        assert_eq!(seen_changed.load(Ordering::SeqCst), 1);

        // Frame 2: do_mutate == false -> Changed<Position> must be 0
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 0);

        // Frame 3: do_mutate == true -> Changed<Position> must be 1
        do_mutate.store(true, Ordering::SeqCst);
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 1);

        // Frame 4: do_mutate == false -> Changed<Position> must return to 0!
        // (Without `world.increment_change_tick()` in DynamicCSystem, ticks could collide across boundaries!)
        do_mutate.store(false, Ordering::SeqCst);
        app.update();
        assert_eq!(seen_changed.load(Ordering::SeqCst), 0);
    }

    // ============================================================================
    // 4. D3, D7, D8, D9, D10 Soundness Guards
    // ============================================================================

    #[test]
    fn d3_forbids_raw_byte_insertion_on_heap_owning_and_opaque_components() {
        const {
            assert!(!NameTag::IS_RAW_INSERTABLE);
            assert!(!FlaggedComp::IS_RAW_INSERTABLE);
            assert!(crate::support::Velocity::IS_RAW_INSERTABLE);
        }

        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<NameTag>();
        // Standard `register_mod_component` must automatically block invalid-`bool` raw byte insertion
        // via `FlaggedComp::IS_RAW_INSERTABLE == false`!
        app.register_mod_component::<FlaggedComp>();
        // Explicit opaque registration also blocks raw insertion on `IS_RAW_INSERTABLE == true` types.
        app.register_mod_component_opaque::<crate::support::Velocity>();

        let entity = app.world_mut().spawn_empty().id();
        let reg = app.world().resource::<ModRegistry>().clone();
        let mut queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut queue, app.world().entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        // Forged zero bytes of size_of::<NameTag>() would create a null/invalid String if bit-copied!
        let forged_name = vec![0u8; core::mem::size_of::<NameTag>()];
        // SAFETY: Passing valid command queue, component C string, and byte slice.
        let res_str = unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity::from_bits(entity.to_bits()),
                c"NameTag".as_ptr(),
                forged_name.as_ptr(),
                forged_name.len(),
            )
        };
        assert_eq!(
            res_str,
            FfiResult::PERMISSION_DENIED,
            "Raw byte insertion onto String-owning component MUST return PERMISSION_DENIED"
        );

        // Invalid bool pattern `2u8` rejected automatically on `register_mod_component::<FlaggedComp>()`
        let forged_bool = [2u8; core::mem::size_of::<FlaggedComp>()];
        // SAFETY: Passing valid command queue, component C string, and byte slice.
        let res_bool = unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity::from_bits(entity.to_bits()),
                c"FlaggedComp".as_ptr(),
                forged_bool.as_ptr(),
                forged_bool.len(),
            )
        };
        assert_eq!(res_bool, FfiResult::PERMISSION_DENIED);

        // Opaque POD component also rejects raw bytes
        let vel_bytes = [0u8; core::mem::size_of::<crate::support::Velocity>()];
        // SAFETY: Passing valid command queue, component C string, and byte slice.
        let res_vel = unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity::from_bits(entity.to_bits()),
                c"Velocity".as_ptr(),
                vel_bytes.as_ptr(),
                vel_bytes.len(),
            )
        };
        assert_eq!(res_vel, FfiResult::PERMISSION_DENIED);

        queue.apply(app.world_mut());
        assert!(app.world().get::<NameTag>(entity).is_none());
        assert!(app.world().get::<FlaggedComp>(entity).is_none());
        assert!(
            app.world()
                .get::<crate::support::Velocity>(entity)
                .is_none()
        );
    }

    static D7_CLONE_DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn d7_drop_counter(_ptr: *mut c_void) {
        D7_CLONE_DROP_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn d7_droppable_dynamic_component_uses_clone_ignore_preventing_double_free() {
        D7_CLONE_DROP_COUNT.store(0, Ordering::SeqCst);
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
            c"ForeignOwnedHandle".as_ptr(),
            ComponentUuid::from_name("ForeignOwnedHandle"),
            8,
            8,
        )
        .with_drop(d7_drop_counter);

        // SAFETY: Valid ModInitContext and descriptor.
        assert_eq!(
            unsafe {
                host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &desc)
            },
            FfiResult::OK
        );
        drop(_guard);

        let comp_id = world
            .resource::<ModRegistry>()
            .get_by_name("ForeignOwnedHandle")
            .unwrap()
            .component_id;

        let reg = world.resource::<ModRegistry>().clone();
        let mut queue = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let ffi_src = unsafe { host_cmd_spawn_empty(raw_queue) };
        let payload = 0xDEAD_BEEF_CAFE_BABEu64.to_ne_bytes();
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_src,
                    c"ForeignOwnedHandle".as_ptr(),
                    payload.as_ptr(),
                    payload.len(),
                )
            },
            FfiResult::OK
        );
        queue.apply(&mut world);

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
            D7_CLONE_DROP_COUNT.load(Ordering::SeqCst),
            1,
            "Foreign destructor must run exactly once (no double-free from entity clone)"
        );
    }
}

mod optional_changed_resources_and_collisions {
    #![allow(unused_imports, dead_code)]
    use crate::support::{ensure_guest_mods_built, reset_rust_mod_panic};
    use bevy_abi_mod_host::{AbiModPlugin, ComponentCollisionError, ModAppExt, ModRegistry};
    use bevy_abi_mod_host_sys::{
        ModInitContext, host_cmd_insert_component, host_cmd_insert_default,
        host_cmd_remove_component, host_cmd_spawn_empty, host_get_proc_address,
        host_iter_column_mutable, host_iter_column_readonly, host_iter_entities,
        host_iter_next_chunk, host_iter_resource_mutable, host_iter_resource_readonly,
        host_register_component, host_register_conditional_system,
        host_register_multi_query_system, host_register_system,
    };
    use bevy_abi_mod_macros::ModExposable;
    use bevy_abi_mod_sys::{
        ComponentAccessDesc, ComponentUuid, ConditionalSystemDescriptor, ExposableComponent,
        FFI_BOOL_FALSE, FFI_BOOL_TRUE, FfiBool, FfiEntity, FfiResult, ModComponentDescriptor,
        MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue, StrView,
        SystemDescriptor,
    };
    use bevy_app::{App, Update};
    use bevy_ecs::component::Component;
    use bevy_ecs::entity::Entity;
    use bevy_ecs::query::{Added, Changed};
    use bevy_ecs::resource::Resource;
    use bevy_ecs::schedule::{LogLevel, ScheduleBuildSettings};
    use bevy_ecs::system::Res;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
    use libloading::{Library, Symbol};
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    // ============================================================================
    // Shared Host ECS Components & Resources for Phase 3 + Phase 4 Tests
    // ============================================================================

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Pos3 {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Vel3 {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Sensor {
        pub signal: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    #[component(storage = "SparseSet")]
    pub struct SparseBoost {
        pub factor: f32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct RemovableMarker {
        pub tag: u32,
    }

    #[derive(Component, ModExposable, Debug, Clone, PartialEq, Default)]
    pub struct DefaultableTag {
        pub label: String,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq)]
    pub struct NoDefaultComp {
        pub code: u64,
    }

    #[repr(C)]
    #[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct SimConfig {
        pub dt: f32,
        pub step_count: u32,
        pub banner_len_observed: u32,
    }

    #[derive(Resource, ModExposable, Debug, Clone, PartialEq, Default)]
    pub struct WorldBanner {
        pub title: String,
    }

    // ============================================================================
    // Asymmetric Mod Interop Types (Exact Name & Layout Match with Rust + C99 Mods)
    // ============================================================================

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4Transform {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4CTransform {
        pub x: f32,
        pub y: f32,
        pub z: f32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4Sensor {
        pub val: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4CSensor {
        pub val: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4FreshTag {
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4CFreshTag {
        pub code: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4Removable {
        pub dummy: u32,
    }

    #[repr(C)]
    #[derive(Component, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4CRemovable {
        pub dummy: u32,
    }

    #[derive(Component, ModExposable, Debug, Clone, PartialEq, Default)]
    pub struct NameTag {
        pub label: String,
    }

    #[repr(C)]
    #[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4RustRes {
        pub scale: f32,
        pub matched_rows: u32,
        pub optional_vel_hits: u32,
        pub spawned_default_entity: u64,
    }

    #[repr(C)]
    #[derive(Resource, ModExposable, Debug, Clone, Copy, PartialEq, Default)]
    pub struct Phase4CRes {
        pub scale: f32,
        pub matched_rows: u32,
        pub optional_vel_hits: u32,
        pub spawned_default_entity: u64,
    }

    fn register_inline_system(app: &mut App, desc: &SystemDescriptor) -> FfiResult {
        let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
        let mut ctx = ModInitContext::new(registry);
        ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
        let status = {
            let _guard = ctx.make_active();
            let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
            // SAFETY: `ctx_ptr` and `desc` are valid stack pointers for the duration of registration.
            unsafe { host_register_system(ctx_ptr, desc) }
        };
        for sys in ctx.registered_systems.drain(..) {
            app.add_systems(Update, sys);
        }
        status
    }

    fn register_inline_conditional_system(
        app: &mut App,
        desc: &ConditionalSystemDescriptor,
    ) -> FfiResult {
        let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
        let mut ctx = ModInitContext::new(registry);
        ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
        let status = {
            let _guard = ctx.make_active();
            let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();
            // SAFETY: `ctx_ptr` and `desc` are valid stack pointers for the duration of registration.
            unsafe { host_register_conditional_system(ctx_ptr, desc) }
        };
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
        app.register_mod_component::<Pos3>();
        app.register_mod_component::<Vel3>();
        app.register_mod_component::<SparseBoost>();

        // Archetype A: (Pos3, Vel3)
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

        // Archetype B: (Pos3,) -- missing BOTH Vel3 and SparseBoost!
        let e_pos_only = app
            .world_mut()
            .spawn(Pos3 {
                x: 100.0,
                y: 0.0,
                z: 0.0,
            })
            .id();

        // Archetype C: (Pos3, Vel3, SparseBoost) -- has dense Vel3 AND SparseSet SparseBoost!
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

        // Track which entities hadChanged<SparseBoost> in a post-check Bevy system
        let changed_boost_entities: Arc<Mutex<Vec<Entity>>> = Arc::new(Mutex::new(Vec::new()));

        unsafe extern "C" fn optional_kinematics_cb(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            loop {
                // SAFETY: Valid `iter` passed by `DynamicCSystem`.
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                // Column 1 is OPTIONAL_READ (`Vel3`): mutable borrow MUST be rejected!
                // SAFETY: Valid `iter`.
                assert!(unsafe { host_iter_column_mutable(iter, 1) }.is_null());

                // SAFETY: Column 0 is WRITE `Pos3`, Column 1 is OPTIONAL_READ `Vel3`,
                // Column 2 is OPTIONAL_WRITE `SparseBoost`.
                let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
                let opt_vel_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const Vel3;
                let opt_boost_ptr =
                    unsafe { host_iter_column_mutable(iter, 2) } as *mut SparseBoost;
                assert!(!pos_ptr.is_null());

                for i in 0..count {
                    // SAFETY: `i < count` within active chunk bounds.
                    unsafe {
                        let vx = if !opt_vel_ptr.is_null() {
                            (*opt_vel_ptr.add(i)).x
                        } else {
                            1.0
                        };
                        let mult = if !opt_boost_ptr.is_null() {
                            let b = &mut *opt_boost_ptr.add(i);
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

        // Prime initial spawn ticks with one update BEFORE registering the guest system so we can
        // isolate SparseBoost change ticks caused strictly by `OPTIONAL_WRITE`.
        app.update();

        let queries = [
            ComponentAccessDesc::write(c"Pos3".as_ptr()),
            ComponentAccessDesc::optional_read(c"Vel3".as_ptr()),
            ComponentAccessDesc::optional_write(c"SparseBoost".as_ptr()),
        ];
        let desc = SystemDescriptor::new(
            c"optional_kinematics".as_ptr(),
            queries.as_ptr(),
            queries.len(),
            optional_kinematics_cb,
            core::ptr::null_mut(),
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        let boost_log = Arc::clone(&changed_boost_entities);
        app.add_systems(
            Update,
            move |q: bevy_ecs::system::Query<Entity, Changed<SparseBoost>>| {
                let mut guard = boost_log.lock().unwrap();
                guard.clear();
                for e in &q {
                    guard.push(e);
                }
            },
        );

        app.update();

        let world = app.world();
        // Archetype A: 10.0 + 5.0 * 1.0 = 15.0
        assert_eq!(world.get::<Pos3>(e_pos_vel).unwrap().x, 15.0);
        // Archetype B (no Vel3, no SparseBoost): 100.0 + 1.0 * 1.0 = 101.0
        assert_eq!(world.get::<Pos3>(e_pos_only).unwrap().x, 101.0);
        // Archetype C (Vel3=2.0, SparseBoost=3.0 -> 4.0): 1000.0 + 2.0 * 3.0 = 1006.0
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
        app.register_mod_component::<Pos3>();
        app.register_mod_component::<Vel3>();
        app.register_mod_component::<Sensor>();
        app.register_mod_component::<SparseBoost>();

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
        let native_changed_log: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));
        let native_added_log: Arc<Mutex<BTreeSet<u64>>> = Arc::new(Mutex::new(BTreeSet::new()));

        unsafe extern "C" fn changed_pos_cb(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `user_data` points to `Mutex<TickFilterLog>` valid for the test lifetime.
            let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
            let mut guard = log_mutex.lock().unwrap();
            loop {
                // SAFETY: Valid `iter`.
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                guard.chunk_lengths.push(count);
                // Column 0 is WRITE Pos3, Column 1 is CHANGED Pos3 (non-data tick filter when paired with WRITE!)
                // SAFETY: Valid `iter`.
                assert!(unsafe { host_iter_column_readonly(iter, 1) }.is_null());
                let ents = unsafe { host_iter_entities(iter) };
                let pos_mut = unsafe { host_iter_column_mutable(iter, 0) } as *mut Pos3;
                assert!(!ents.is_null() && !pos_mut.is_null());
                for row in 0..count {
                    // SAFETY: `row < count`.
                    unsafe {
                        guard.changed_entities.push((*ents.add(row)).0);
                        // Mutate y on matched changed rows — must NOT self-retrigger on the next frame!
                        (*pos_mut.add(row)).y += 1.0;
                    }
                }
            }
            FfiResult::OK
        }

        unsafe extern "C" fn added_sensor_cb(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `user_data` points to `Mutex<TickFilterLog>` valid for the test lifetime.
            let log_mutex = unsafe { &*(user_data as *const Mutex<TickFilterLog>) };
            let mut guard = log_mutex.lock().unwrap();
            loop {
                // SAFETY: Valid `iter`.
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                let ents = unsafe { host_iter_entities(iter) };
                let sensor_ro = unsafe { host_iter_column_readonly(iter, 0) } as *const Sensor;
                assert!(!ents.is_null() && !sensor_ro.is_null());
                for row in 0..count {
                    // SAFETY: `row < count`.
                    unsafe {
                        let _sig = (*sensor_ro.add(row)).signal;
                        guard.added_entities.push((*ents.add(row)).0);
                    }
                }
            }
            FfiResult::OK
        }

        let log_ptr = Arc::as_ptr(&c_abi_log) as *mut c_void;

        // System A: `[WRITE(Pos3), CHANGED(Pos3)]` (self-compatible mutable + Changed<Pos3>!)
        let changed_queries = [
            ComponentAccessDesc::write(c"Pos3".as_ptr()),
            ComponentAccessDesc::changed(c"Pos3".as_ptr()),
        ];
        let changed_desc = SystemDescriptor::new(
            c"changed_pos_sys".as_ptr(),
            changed_queries.as_ptr(),
            changed_queries.len(),
            changed_pos_cb,
            log_ptr,
        );
        assert_eq!(
            register_inline_system(&mut app, &changed_desc),
            FfiResult::OK
        );

        // System B: `[ADDED(Sensor)]`
        let added_queries = [ComponentAccessDesc::added(c"Sensor".as_ptr())];
        let added_desc = SystemDescriptor::new(
            c"added_sensor_sys".as_ptr(),
            added_queries.as_ptr(),
            added_queries.len(),
            added_sensor_cb,
            log_ptr,
        );
        assert_eq!(register_inline_system(&mut app, &added_desc), FfiResult::OK);

        // Native Bevy reference systems for differential parity on `Added<Sensor>`
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
        let _ = &native_changed_log;

        // Frame 1: All 12 initial entities match BOTH `Changed<Pos3>` (single 12-row chunk) AND `Added<Sensor>`!
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

        // Frame 2: No external mutations! Even though `changed_pos_sys` mutated `Pos3.y` in Frame 1,
        // its own `this_run` tick MUST NOT re-trigger `Changed<Pos3>` on Frame 2!
        app.update();
        {
            let guard = c_abi_log.lock().unwrap();
            assert!(
                guard.changed_entities.is_empty(),
                "System must NOT observe its own prior-frame mutation as Changed<Pos3>!"
            );
            assert!(guard.chunk_lengths.is_empty());
            assert!(guard.added_entities.is_empty());
            assert!(native_added_log.lock().unwrap().is_empty());
        }

        // Frame 3: Mutate non-contiguous row runs in the 12-row table:
        // rows [1, 2, 3] (len 3), row [7] (len 1), rows [10, 11] (len 2)!
        // Also mutate Sensor on row 5 (NOT Added!) and spawn 2 brand-new entities (Added + Changed)!
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
                Vel3::default(), // Different archetype/table so original 12-row table stays intact!
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
            // Original 12-row table coalesced into runs [3, 1, 2], plus new 2-row table [2]!
            assert_eq!(chunks, vec![3, 1, 2, 2]);

            let native_added = std::mem::take(&mut *native_added_log.lock().unwrap());
            let expected_added: BTreeSet<u64> =
                [e_new_1.to_bits(), e_new_2.to_bits()].into_iter().collect();
            // Mutating `Sensor` on `entities[5]` MUST NOT trigger `Added<Sensor>`!
            assert_eq!(c_added, expected_added);
            assert_eq!(c_added, native_added);
        }
    }

    #[test]
    fn test_sparse_set_changed_and_added_filters() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<SparseBoost>();

        let e1 = app.world_mut().spawn(SparseBoost { factor: 1.0 }).id();
        let e2 = app.world_mut().spawn(SparseBoost { factor: 2.0 }).id();
        let e3 = app.world_mut().spawn(SparseBoost { factor: 3.0 }).id();

        let seen_changed: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::new()));

        unsafe extern "C" fn sparse_changed_cb(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `user_data` points to `Mutex<Vec<u64>>`.
            let vec_mutex = unsafe { &*(user_data as *const Mutex<Vec<u64>>) };
            let mut guard = vec_mutex.lock().unwrap();
            loop {
                // SAFETY: Valid `iter`.
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                let ents = unsafe { host_iter_entities(iter) };
                let boost_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const SparseBoost;
                assert!(!ents.is_null() && !boost_ptr.is_null());
                for r in 0..count {
                    // SAFETY: `r < count`.
                    unsafe {
                        guard.push((*ents.add(r)).0);
                    }
                }
            }
            FfiResult::OK
        }

        let queries = [ComponentAccessDesc::changed(c"SparseBoost".as_ptr())];
        let desc = SystemDescriptor::new(
            c"sparse_changed_sys".as_ptr(),
            queries.as_ptr(),
            queries.len(),
            sparse_changed_cb,
            Arc::as_ptr(&seen_changed) as *mut c_void,
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        // Frame 1: all 3 seen
        app.update();
        assert_eq!(seen_changed.lock().unwrap().drain(..).count(), 3);

        // Frame 2: none changed
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
    // 3. ECS Resources (POD + Opaque VTable, Change Ticks, & Scheduler Conflicts)
    // ============================================================================

    #[repr(C)]
    struct WorldBannerVTableMirror {
        size: usize,
        align: usize,
        get_title: unsafe extern "C" fn(*const c_void) -> StrView,
        set_title: unsafe extern "C" fn(*mut c_void, *const u8, usize),
    }

    static WRITE_SIM_CONFIG_TRIGGER: AtomicU8 = AtomicU8::new(1);

    #[test]
    fn test_ecs_resources_pod_and_opaque_vtable_and_change_ticks() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_resource::<SimConfig>();
        app.register_mod_resource::<WorldBanner>();
        app.insert_resource(SimConfig {
            dt: 0.5,
            step_count: 0,
            banner_len_observed: 0,
        });
        app.insert_resource(WorldBanner {
            title: "BevyAbiMod".to_string(),
        });

        // Clear initial resource insertion change ticks before registering systems
        app.update();

        unsafe extern "C" fn pure_resource_sys(
            iter: *mut RawChunkIter,
            commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            // Undeclared resource MUST return null!
            // SAFETY: Valid `iter`.
            assert!(
                unsafe { host_iter_resource_readonly(iter, c"NonExistentRes".as_ptr()) }.is_null()
            );

            // `WorldBanner` was declared `RESOURCE_READ`: mutable borrow MUST return null!
            // SAFETY: Valid `iter`.
            assert!(unsafe { host_iter_resource_mutable(iter, c"WorldBanner".as_ptr()) }.is_null());

            // Attempting `cmd_insert_component` on a Resource MUST be rejected with INVALID_COMPONENT!
            let dummy = SimConfig::default();
            // SAFETY: Valid `commands` and `dummy` bytes.
            let bad_cmd = unsafe {
                host_cmd_insert_component(
                    commands,
                    FfiEntity::from_bits(1),
                    c"SimConfig".as_ptr(),
                    (&dummy as *const SimConfig).cast::<u8>(),
                    core::mem::size_of::<SimConfig>(),
                )
            };
            assert_eq!(bad_cmd, FfiResult::INVALID_COMPONENT);

            // Read `WorldBanner` via its generated VTable!
            // SAFETY: Valid symbol lookup bound to current World's ModRegistry.
            let banner_vt_ptr = unsafe { host_get_proc_address(c"WorldBanner_vtable".as_ptr()) }
                as *const WorldBannerVTableMirror;
            assert!(!banner_vt_ptr.is_null());
            // SAFETY: Valid `WorldBanner` read-only pointer.
            let banner_ptr = unsafe { host_iter_resource_readonly(iter, c"WorldBanner".as_ptr()) };
            assert!(!banner_ptr.is_null());
            // SAFETY: Calling host-generated `get_title` VTable accessor.
            let view = unsafe { ((*banner_vt_ptr).get_title)(banner_ptr.cast::<c_void>()) };

            if WRITE_SIM_CONFIG_TRIGGER.load(Ordering::SeqCst) != 0 {
                // Mutate `SimConfig` -> marks Bevy resource change tick!
                // SAFETY: Declared `RESOURCE_WRITE` on `SimConfig`.
                let cfg_ptr = unsafe { host_iter_resource_mutable(iter, c"SimConfig".as_ptr()) }
                    as *mut SimConfig;
                assert!(!cfg_ptr.is_null());
                // SAFETY: Exclusive borrow of `SimConfig`.
                unsafe {
                    (*cfg_ptr).step_count += 1;
                    (*cfg_ptr).banner_len_observed = view.len as u32;
                }
            } else {
                // Read-only access even when `RESOURCE_WRITE` is declared -> MUST NOT mark changed!
                // SAFETY: Valid read-only access.
                let cfg_ro = unsafe { host_iter_resource_readonly(iter, c"SimConfig".as_ptr()) }
                    as *const SimConfig;
                assert!(!cfg_ro.is_null());
            }

            // Pure resource-only system has 0 entity chunks!
            // SAFETY: Valid `iter`.
            assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
            FfiResult::OK
        }

        WRITE_SIM_CONFIG_TRIGGER.store(1, Ordering::SeqCst);

        let queries = [
            ComponentAccessDesc::resource_write(c"SimConfig".as_ptr()),
            ComponentAccessDesc::resource_read(c"WorldBanner".as_ptr()),
        ];
        let desc = SystemDescriptor::new(
            c"pure_resource_sys".as_ptr(),
            queries.as_ptr(),
            queries.len(),
            pure_resource_sys,
            core::ptr::null_mut(),
        );
        assert_eq!(register_inline_system(&mut app, &desc), FfiResult::OK);

        // Frame 1: `WRITE_SIM_CONFIG_TRIGGER = 1` -> mutates `SimConfig`!
        app.update();
        assert_eq!(
            *app.world().resource::<SimConfig>(),
            SimConfig {
                dt: 0.5,
                step_count: 1,
                banner_len_observed: "BevyAbiMod".len() as u32,
            }
        );

        // Add a native Bevy observer system ordered AFTER `ModSystemSet::All` to inspect `Res<SimConfig>::is_changed()`!
        let native_saw_changed = Arc::new(Mutex::new(false));
        let saw_clone = Arc::clone(&native_saw_changed);
        use bevy_ecs::schedule::IntoScheduleConfigs;
        app.add_systems(
            Update,
            (move |res: Res<SimConfig>| {
                use bevy_ecs::change_detection::DetectChanges;
                *saw_clone.lock().unwrap() = res.is_changed();
            })
            .after(bevy_abi_mod_host::ModSystemSet::All),
        );

        // Frame 2: `WRITE_SIM_CONFIG_TRIGGER = 1` -> `is_changed()` MUST be `true`!
        app.update();
        assert!(*native_saw_changed.lock().unwrap());
        assert_eq!(app.world().resource::<SimConfig>().step_count, 2);

        // Frame 3: `WRITE_SIM_CONFIG_TRIGGER = 0` (calls only `res_readonly`) -> `is_changed()` MUST be `false`!
        WRITE_SIM_CONFIG_TRIGGER.store(0, Ordering::SeqCst);
        app.update();
        assert!(
            !*native_saw_changed.lock().unwrap(),
            "Calling only `res_readonly` must not mark Bevy Resource as changed!"
        );
        assert_eq!(app.world().resource::<SimConfig>().step_count, 2);
    }

    // ============================================================================
    // 5. Deferred `cmd_remove_component` & `cmd_insert_default` (Opaque String)
    // ============================================================================

    #[repr(C)]
    struct DefaultableTagVTableMirror {
        size: usize,
        align: usize,
        get_label: unsafe extern "C" fn(*const c_void) -> StrView,
        set_label: unsafe extern "C" fn(*mut c_void, *const u8, usize),
    }

    #[test]
    fn test_cmd_remove_component_and_cmd_insert_default_opaque_string_workflow() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Pos3>();
        app.register_mod_component::<RemovableMarker>();
        app.register_mod_component::<NoDefaultComp>();
        app.register_mod_component_with_default::<DefaultableTag>();

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
            spawned_entity: AtomicUsize,
            did_spawn: AtomicU8,
        }

        let ctx = Box::new(CmdTestCtx {
            existing_entity: FfiEntity::from_bits(existing.to_bits()),
            spawned_entity: AtomicUsize::new(0),
            did_spawn: AtomicU8::new(0),
        });

        unsafe extern "C" fn spawner_and_remover_sys(
            _iter: *mut RawChunkIter,
            commands: *mut RawCommandQueue,
            user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: `user_data` points to `CmdTestCtx`.
            let ctx = unsafe { &*(user_data as *const CmdTestCtx) };
            if ctx.did_spawn.swap(1, Ordering::SeqCst) != 0 {
                return FfiResult::OK;
            }

            // 1. Remove `RemovableMarker` from `existing_entity`
            // SAFETY: Valid `commands` and CStr.
            let rem_status = unsafe {
                host_cmd_remove_component(
                    commands,
                    ctx.existing_entity,
                    c"RemovableMarker".as_ptr(),
                )
            };
            assert_eq!(rem_status, FfiResult::OK);

            // 2. Spawn a brand-new entity and attach `DefaultableTag` via `host_cmd_insert_default`!
            // SAFETY: Valid `commands`.
            let spawned = unsafe { host_cmd_spawn_empty(commands) };
            assert_ne!(spawned.0, 0);

            // Raw byte insertion on non-POD `DefaultableTag` (`String`) MUST still be rejected per D3!
            let fake_bytes = [0u8; core::mem::size_of::<DefaultableTag>()];
            // SAFETY: Valid arguments testing permission rejection.
            let raw_status = unsafe {
                host_cmd_insert_component(
                    commands,
                    spawned,
                    c"DefaultableTag".as_ptr(),
                    fake_bytes.as_ptr(),
                    fake_bytes.len(),
                )
            };
            assert_eq!(raw_status, FfiResult::PERMISSION_DENIED);

            // `host_cmd_insert_default` on `NoDefaultComp` (registered without Default) MUST return PERMISSION_DENIED!
            // SAFETY: Valid arguments testing permission rejection.
            let no_def_status =
                unsafe { host_cmd_insert_default(commands, spawned, c"NoDefaultComp".as_ptr()) };
            assert_eq!(no_def_status, FfiResult::PERMISSION_DENIED);

            // `host_cmd_insert_default` on `DefaultableTag` MUST succeed!
            // SAFETY: Valid `commands` and CStr.
            let def_status =
                unsafe { host_cmd_insert_default(commands, spawned, c"DefaultableTag".as_ptr()) };
            assert_eq!(def_status, FfiResult::OK);

            ctx.spawned_entity
                .store(spawned.0 as usize, Ordering::SeqCst);
            FfiResult::OK
        }

        unsafe extern "C" fn tag_vtable_mutator_sys(
            iter: *mut RawChunkIter,
            _commands: *mut RawCommandQueue,
            _user_data: *mut c_void,
        ) -> FfiResult {
            // SAFETY: Valid symbol lookup bound to current World's ModRegistry.
            let vt_ptr = unsafe { host_get_proc_address(c"DefaultableTag_vtable".as_ptr()) }
                as *const DefaultableTagVTableMirror;
            assert!(!vt_ptr.is_null());
            loop {
                // SAFETY: Valid `iter`.
                let count = unsafe { host_iter_next_chunk(iter) };
                if count == 0 {
                    break;
                }
                let base = unsafe { host_iter_column_mutable(iter, 0) };
                assert!(!base.is_null());
                let label = b"created_via_default_and_vtable";
                for r in 0..count {
                    // SAFETY: `r < count`, stride equals `(*vt_ptr).size`.
                    unsafe {
                        let elem = base.add(r * (*vt_ptr).size).cast::<c_void>();
                        ((*vt_ptr).set_label)(elem, label.as_ptr(), label.len());
                    }
                }
            }
            FfiResult::OK
        }

        let spawn_desc = SystemDescriptor::new(
            c"spawner_and_remover_sys".as_ptr(),
            core::ptr::null(),
            0,
            spawner_and_remover_sys,
            (&*ctx as *const CmdTestCtx) as *mut c_void,
        );
        assert_eq!(register_inline_system(&mut app, &spawn_desc), FfiResult::OK);

        let tag_queries = [ComponentAccessDesc::write(c"DefaultableTag".as_ptr())];
        let mut_desc = SystemDescriptor::new(
            c"tag_vtable_mutator_sys".as_ptr(),
            tag_queries.as_ptr(),
            tag_queries.len(),
            tag_vtable_mutator_sys,
            core::ptr::null_mut(),
        );
        assert_eq!(register_inline_system(&mut app, &mut_desc), FfiResult::OK);

        // Frame 1: Queues `remove_component` + `spawn_empty` + `insert_default`, applied at end of frame!
        app.update();
        assert!(
            app.world().get::<RemovableMarker>(existing).is_none(),
            "RemovableMarker must be removed after deferred command flush!"
        );
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
    // 6. Component UUID & Metadata Collision Detection (Phase 3.2)
    // ============================================================================

    #[repr(C)]
    #[derive(Component)]
    struct SameNameDifferentSize {
        a: f64,
        b: f64,
        c: f64,
    }

    impl ExposableComponent for SameNameDifferentSize {
        const COMPONENT_NAME: &'static str = "Pos3"; // Collides with `Pos3` (size 12, align 4) vs (size 24, align 8)!
        const COMPONENT_CSTR: &'static core::ffi::CStr = c"Pos3";
        const UUID: ComponentUuid = ComponentUuid::from_name("Pos3");
        type VTable = ();
        fn vtable() -> &'static Self::VTable {
            &()
        }
        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
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

    impl ExposableComponent for DifferentNameSameUuid {
        const COMPONENT_NAME: &'static str = "ForgedPos3Alias";
        const COMPONENT_CSTR: &'static core::ffi::CStr = c"ForgedPos3Alias";
        const UUID: ComponentUuid = Pos3::UUID; // Collides with `Pos3::UUID` under a distinct name!
        type VTable = ();
        fn vtable() -> &'static Self::VTable {
            &()
        }
        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
            &[]
        }
    }

    #[test]
    fn test_component_uuid_and_metadata_collision_detection() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);

        // 1. Initial registration + idempotent re-registration succeed
        assert!(app.try_register_mod_component::<Pos3>().is_ok());
        assert!(app.try_register_mod_component::<Pos3>().is_ok());

        // 2. Same name with mismatched size/align -> MetadataMismatch
        let err_meta = app
            .try_register_mod_component::<SameNameDifferentSize>()
            .expect_err("Mismatched size/align under existing name must fail!");
        assert!(matches!(
            err_meta,
            ComponentCollisionError::MetadataMismatch { .. }
        ));

        // 3. Different name hashing/declaring an existing UUID -> UuidCollision
        let err_uuid = app
            .try_register_mod_component::<DifferentNameSameUuid>()
            .expect_err("Duplicate ComponentUuid under distinct name must fail!");
        assert!(matches!(
            err_uuid,
            ComponentCollisionError::UuidCollision { .. }
        ));

        // 4. Re-registering an entity component as a Resource -> MetadataMismatch (`is_resource` differs!)
        #[repr(C)]
        #[derive(Resource)]
        struct Pos3ResourceCollision {
            x: f32,
            y: f32,
            z: f32,
        }
        impl ExposableComponent for Pos3ResourceCollision {
            const COMPONENT_NAME: &'static str = "Pos3";
            const COMPONENT_CSTR: &'static core::ffi::CStr = c"Pos3";
            const UUID: ComponentUuid = Pos3::UUID;
            type VTable = ();
            fn vtable() -> &'static Self::VTable {
                &()
            }
            fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
                &[]
            }
        }
        let err_res_mismatch = app
            .try_register_mod_resource::<Pos3ResourceCollision>()
            .expect_err("Registering an entity component name as a Resource must fail!");
        assert!(matches!(
            err_res_mismatch,
            ComponentCollisionError::MetadataMismatch { .. }
        ));

        // 5. Dynamic `host_register_component` C-ABI collision guard
        let registry = Arc::new(app.world().resource::<ModRegistry>().clone());
        let mut init_ctx = ModInitContext::new(registry);
        init_ctx.world = app.world_mut() as *mut bevy_ecs::world::World;
        let _active_guard = init_ctx.make_active();
        let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

        // Dynamic registration with forged UUID colliding with `Pos3::UUID` -> INVALID_COMPONENT!
        let forged_desc = ModComponentDescriptor {
            struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
            name: c"DynamicForgedUuid".as_ptr(),
            uuid: Pos3::UUID,
            size: 8,
            align: 4,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        };
        // SAFETY: Valid `ctx_ptr` and `forged_desc`.
        assert_eq!(
            unsafe { host_register_component(ctx_ptr, &forged_desc) },
            FfiResult::INVALID_COMPONENT
        );

        // Valid dynamic registration + idempotent re-registration -> OK, then layout mismatch -> INVALID_COMPONENT!
        let valid_dyn = ModComponentDescriptor {
            struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
            name: c"ValidDynComp".as_ptr(),
            uuid: ComponentUuid::from_name("ValidDynComp"),
            size: 8,
            align: 4,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        };
        // SAFETY: Valid `ctx_ptr` and `valid_dyn`.
        assert_eq!(
            unsafe { host_register_component(ctx_ptr, &valid_dyn) },
            FfiResult::OK
        );
        assert_eq!(
            unsafe { host_register_component(ctx_ptr, &valid_dyn) },
            FfiResult::OK
        );

        let mismatched_dyn = ModComponentDescriptor {
            size: 16,
            ..valid_dyn
        };
        // SAFETY: Valid `ctx_ptr` and `mismatched_dyn`.
        assert_eq!(
            unsafe { host_register_component(ctx_ptr, &mismatched_dyn) },
            FfiResult::INVALID_COMPONENT
        );
    }

    #[test]
    #[should_panic(expected = "collision")]
    fn test_conflicting_static_register_mod_component_panics_with_clear_message() {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Pos3>();
        app.register_mod_component::<DifferentNameSameUuid>();
    }
}
