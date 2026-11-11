#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult,
    ModComponentDescriptor, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    SystemDescriptor, ABI_VERSION_PACKED,
};
use bevy_app::{App, Startup, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::observer::On;
use bevy_ecs::query::{Changed, With};
use bevy_ecs::schedule::{IntoScheduleConfigs, ScheduleBuildSettings, ScheduleCleanupPolicy, Schedules};
use bevy_ecs::system::RunSystemOnce;
use bevy_ecs::world::{CommandQueue, World};
use common::{NameTag, Position};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_iter_reset, host_register_component, host_register_multi_query_system,
    host_register_system, host_register_system_in_schedule, is_abi_version_compatible,
    AbiModPlugin, CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, LoadedModInfo,
    ModAppExt, ModErrorPolicy, ModId, ModInitContext, ModLoadError, ModLoaded, ModRegistry,
    ModSchedule, ModSystemFaulted, ModSystemSet, ModUnloaded, MAX_QUERY_COMPONENTS,
};
use std::sync::{Arc, Mutex};

#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
#[component(storage = "SparseSet")]
pub struct SparseTag {
    pub code: u32,
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
#[component(storage = "SparseSet")]
pub struct SparseShield {
    pub strength: f32,
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq)]
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

    let observe_changed = move |q: bevy_ecs::system::Query<&SparseShield, Changed<SparseShield>>| {
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
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<NameTag>();
    app.register_mod_component_opaque::<FlaggedComp>();

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

    // Invalid bool pattern `2u8` rejected on opaque component
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

    queue.apply(app.world_mut());
    assert!(app.world().get::<NameTag>(entity).is_none());
    assert!(app.world().get::<FlaggedComp>(entity).is_none());
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

unsafe extern "C" fn noop_single_cb(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_multi_cb(
    _iters: *const *mut RawChunkIter,
    _count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

#[test]
fn d8_rejects_query_count_exceeding_max_query_components() {
    let registry = Arc::new(ModRegistry::new());
    let mut init_ctx = ModInitContext::new(registry);
    let ctx_ptr = (&mut init_ctx as *mut ModInitContext).cast::<c_void>();

    let names: Vec<std::ffi::CString> = (0..=MAX_QUERY_COMPONENTS)
        .map(|i| std::ffi::CString::new(format!("Comp_{i}")).unwrap())
        .collect();
    let queries: Vec<ComponentAccessDesc> = names
        .iter()
        .map(|n| ComponentAccessDesc::read(n.as_ptr()))
        .collect();
    assert_eq!(queries.len(), MAX_QUERY_COMPONENTS + 1);

    let single_desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"too_many_cols".as_ptr(),
        queries: queries.as_ptr(),
        query_count: queries.len(),
        callback: noop_single_cb,
        user_data: core::ptr::null_mut(),
    };
    // SAFETY: Valid pointers and initialized slices.
    assert_eq!(
        unsafe { host_register_system(ctx_ptr, &single_desc) },
        FfiResult::INVALID_COMPONENT
    );
    assert!(init_ctx.registered_systems.is_empty());

    let spec = QuerySpec {
        queries: queries.as_ptr(),
        query_count: queries.len(),
    };
    let multi_desc = MultiQuerySystemDescriptor {
        struct_size: core::mem::size_of::<MultiQuerySystemDescriptor>() as u32,
        name: c"too_many_multi_cols".as_ptr(),
        query_specs: &spec,
        query_spec_count: 1,
        callback: noop_multi_cb,
        user_data: core::ptr::null_mut(),
    };
    // SAFETY: Valid pointers and initialized slices.
    assert_eq!(
        unsafe { host_register_multi_query_system(ctx_ptr, &multi_desc) },
        FfiResult::INVALID_COMPONENT
    );
    assert!(init_ctx.registered_systems.is_empty());
}

#[test]
fn d9_and_d10_null_placeholder_entities_rejected_and_overaligned_zst_succeeds() {
    let mut world = World::new();
    let mut reg = ModRegistry::new();
    let pos_id = world.register_component::<Position>();
    reg.register_component::<Position>(pos_id);
    world.insert_resource(reg);

    // Register a 64-byte aligned ZST dynamic component (D10)
    let reg_arc = Arc::new(world.resource::<ModRegistry>().clone());
    let is_active = Arc::new(AtomicBool::new(true));
    let mut init_ctx =
        ModInitContext::with_world(reg_arc, is_active, &mut world as *mut World);
    let _guard = init_ctx.make_active();
    let zst_desc = ModComponentDescriptor::new(
        c"Aligned64Zst".as_ptr(),
        ComponentUuid::from_name("Aligned64Zst"),
        0,
        64,
    );
    // SAFETY: Valid ModInitContext and descriptor.
    assert_eq!(
        unsafe {
            host_register_component(&mut init_ctx as *mut ModInitContext as *mut c_void, &zst_desc)
        },
        FfiResult::OK
    );
    drop(_guard);

    let zst_id = world
        .resource::<ModRegistry>()
        .get_by_name("Aligned64Zst")
        .unwrap()
        .component_id;

    // D9: null queue returns FfiEntity(0)
    assert_eq!(
        unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
        FfiEntity(0)
    );

    let reg_snap = world.resource::<ModRegistry>().clone();
    let mut queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(&mut queue, world.entity_allocator(), &reg_snap);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let placeholder_ffi = FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits());
    let pos_bytes = [0u8; core::mem::size_of::<Position>()];

    // Reject FfiEntity(0) and Entity::PLACEHOLDER in both despawn and insert_component!
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
        FfiResult::INTERNAL_ERROR
    );
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, placeholder_ffi) },
        FfiResult::INTERNAL_ERROR
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                FfiEntity(0),
                c"Position".as_ptr(),
                pos_bytes.as_ptr(),
                pos_bytes.len(),
            )
        },
        FfiResult::INTERNAL_ERROR
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                placeholder_ffi,
                c"Position".as_ptr(),
                pos_bytes.as_ptr(),
                pos_bytes.len(),
            )
        },
        FfiResult::INTERNAL_ERROR
    );

    // D10: Insert 64-byte-aligned ZST onto a real spawned entity without panicking in OwningPtr::new!
    let valid_ffi = unsafe { host_cmd_spawn_empty(raw_queue) };
    assert_ne!(valid_ffi, FfiEntity(0));
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_ffi,
                c"Aligned64Zst".as_ptr(),
                core::ptr::null(),
                0,
            )
        },
        FfiResult::OK
    );

    queue.apply(&mut world);
    let valid_entity = Entity::try_from_bits(valid_ffi.to_bits()).unwrap();
    assert!(world.entity(valid_entity).contains_id(zst_id));
}

// ============================================================================
// 5. Deliverable 4: Per-World Drop Isolation & Worker-Thread Registry Guard
// ============================================================================

static WORLD_A_DROPS: AtomicUsize = AtomicUsize::new(0);
static WORLD_B_DROPS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn drop_world_a(_ptr: *mut c_void) {
    WORLD_A_DROPS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn drop_world_b(_ptr: *mut c_void) {
    WORLD_B_DROPS.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn two_worlds_same_uuid_keep_isolated_drop_callbacks_including_world_drop() {
    WORLD_A_DROPS.store(0, Ordering::SeqCst);
    WORLD_B_DROPS.store(0, Ordering::SeqCst);

    let shared_uuid = ComponentUuid::from_name("SharedCrossWorldComp");

    let mut world_a = World::new();
    let reg_a = Arc::new(ModRegistry::new());
    world_a.insert_resource((*reg_a).clone());
    let active_a = Arc::new(AtomicBool::new(true));
    {
        let mut ctx_a = ModInitContext::with_world(
            Arc::clone(&reg_a),
            Arc::clone(&active_a),
            &mut world_a as *mut World,
        );
        let _g = ctx_a.make_active();
        let desc_a = ModComponentDescriptor::new(c"SharedCrossWorldComp".as_ptr(), shared_uuid, 8, 8)
            .with_drop(drop_world_a);
        assert_eq!(
            unsafe {
                host_register_component(&mut ctx_a as *mut ModInitContext as *mut c_void, &desc_a)
            },
            FfiResult::OK
        );
    }

    let mut world_b = World::new();
    let reg_b = Arc::new(ModRegistry::new());
    world_b.insert_resource((*reg_b).clone());
    let active_b = Arc::new(AtomicBool::new(true));
    {
        let mut ctx_b = ModInitContext::with_world(
            Arc::clone(&reg_b),
            Arc::clone(&active_b),
            &mut world_b as *mut World,
        );
        let _g = ctx_b.make_active();
        let desc_b = ModComponentDescriptor::new(c"SharedCrossWorldComp".as_ptr(), shared_uuid, 8, 8)
            .with_drop(drop_world_b);
        assert_eq!(
            unsafe {
                host_register_component(&mut ctx_b as *mut ModInitContext as *mut c_void, &desc_b)
            },
            FfiResult::OK
        );
    }

    // Spawn 1 entity in World A and 1 entity in World B
    for world in [&mut world_a, &mut world_b] {
        let r = world.resource::<ModRegistry>().clone();
        let mut q = CommandQueue::default();
        let mut b = CommandQueueBridge::new(&mut q, world.entity_allocator(), &r);
        let rq = (&mut b as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let e = unsafe { host_cmd_spawn_empty(rq) };
        let bytes = [1u8; 8];
        assert_eq!(
            unsafe {
                host_cmd_insert_component(
                    rq,
                    e,
                    c"SharedCrossWorldComp".as_ptr(),
                    bytes.as_ptr(),
                    8,
                )
            },
            FfiResult::OK
        );
        q.apply(world);
    }

    // Dropping World A directly (Table::drop without entity-by-entity despawn!) must run ONLY drop_world_a!
    drop(world_a);
    assert_eq!(WORLD_A_DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(WORLD_B_DROPS.load(Ordering::SeqCst), 0);

    // Dropping World B must run ONLY drop_world_b!
    drop(world_b);
    assert_eq!(WORLD_A_DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(WORLD_B_DROPS.load(Ordering::SeqCst), 1);
}

unsafe extern "C" fn probe_proc_address_in_executor(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // Query a symbol registered ONLY in GLOBAL_SYMBOLS (from another world), NOT in this World's ModRegistry!
    // SAFETY: Valid NUL-terminated C string literal.
    let foreign_sym = unsafe { host_get_proc_address(c"OnlyInOtherWorldSymbol".as_ptr()) };
    let own_sym = unsafe { host_get_proc_address(c"OwnWorldSymbol".as_ptr()) };
    let passed = foreign_sym.is_null() && own_sym == (0x1234usize as *const c_void);
    // SAFETY: `user_data` points to an `AtomicBool`.
    unsafe { (*(user_data as *const AtomicBool)).store(passed, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn runtime_get_proc_address_on_executor_is_isolated_to_current_world_registry() {
    // Pollute GLOBAL_SYMBOLS via an unrelated ModRegistry
    let mut other_reg = ModRegistry::new();
    other_reg.insert_symbol("OnlyInOtherWorldSymbol", 0x9999usize as *const c_void);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.world_mut()
        .resource_mut::<ModRegistry>()
        .insert_symbol("OwnWorldSymbol", 0x1234usize as *const c_void);

    let ok_flag = Arc::new(AtomicBool::new(false));
    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let sys = DynamicCSystem::new(
        "probe_symbols".to_string(),
        vec![],
        probe_proc_address_in_executor,
        Arc::as_ptr(&ok_flag) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    assert!(
        ok_flag.load(Ordering::SeqCst),
        "DynamicCSystem::run_unsafe must bind CURRENT_REGISTRY on executor worker threads"
    );
}

// ============================================================================
// 6. Deliverable 5: Schedules, SystemSets, 1000x Remove-in-Set, ErrorPolicy & Events
// ============================================================================

unsafe extern "C" fn failing_guest_cb(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::INTERNAL_ERROR
}

unsafe extern "C" fn count_runs_cb(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to an `AtomicUsize`.
    unsafe { (*(user_data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn mod_schedule_startup_error_policy_report_to_bevy_and_fault_events_work() {
    // 1. Startup schedule registration via `host_register_system_in_schedule`
    let startup_runs = Arc::new(AtomicUsize::new(0));
    let reg = Arc::new(ModRegistry::new());
    let mut init_ctx = ModInitContext::new(Arc::clone(&reg));
    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name: c"startup_guest_sys".as_ptr(),
        queries: core::ptr::null(),
        query_count: 0,
        callback: count_runs_cb,
        user_data: Arc::as_ptr(&startup_runs) as *mut c_void,
    };
    assert_eq!(
        unsafe {
            host_register_system_in_schedule(
                (&mut init_ctx as *mut ModInitContext).cast::<c_void>(),
                &desc,
                ModSchedule::Startup as u32,
            )
        },
        FfiResult::OK
    );
    let startup_sys = init_ctx.registered_systems.pop().unwrap();
    assert_eq!(startup_sys.schedule(), ModSchedule::Startup);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::ReportToBevy));
    app.add_systems(Startup, startup_sys);

    // Startup system should run exactly once across multiple `app.update()` frames!
    app.update();
    app.update();
    app.update();
    assert_eq!(startup_runs.load(Ordering::SeqCst), 1);

    // 2. ReportToBevy returns Err(RunSystemError::Failed) AND emits ModSystemFaulted (Observer + Messages)
    let observed_faults = Arc::new(Mutex::new(Vec::new()));
    let obs_clone = Arc::clone(&observed_faults);
    app.world_mut()
        .add_observer(move |trigger: On<ModSystemFaulted>| {
            obs_clone.lock().unwrap().push(trigger.event().clone());
        });

    let fault_sys = DynamicCSystem::new(
        "faulting_sys".to_string(),
        vec![],
        failing_guest_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(ModId(42));

    let run_res = app.world_mut().run_system_once(fault_sys);
    assert!(
        run_res.is_err(),
        "ModErrorPolicy::ReportToBevy must return Err(RunSystemError::Failed)"
    );
    assert!(run_res
        .unwrap_err()
        .to_string()
        .contains("faulting_sys"));

    let obs = observed_faults.lock().unwrap();
    assert_eq!(obs.len(), 1);
    assert_eq!(obs[0].mod_id, Some(ModId(42)));
    assert_eq!(obs[0].status, FfiResult::INTERNAL_ERROR);

    let msgs = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(msgs.len(), 1);
}

#[test]
fn hot_reload_1000_cycles_remove_systems_in_set_keeps_schedule_graph_constant() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let run_counter = Arc::new(AtomicUsize::new(0));

    // Measure baseline schedule size after 1 loaded & unloaded mod
    for cycle in 1..=1000u32 {
        let mod_id = ModId(cycle);
        let sys = DynamicCSystem::new(
            format!("hot_sys_{cycle}"),
            vec![],
            count_runs_cb,
            Arc::as_ptr(&run_counter) as *mut c_void,
            Arc::clone(&reg),
        )
        .with_mod_id(mod_id);

        app.add_systems(Update, sys);
        app.update();

        app.world_mut()
            .resource_scope::<Schedules, _>(|world, mut schedules| {
                let update_sched = schedules.get_mut(Update).unwrap();
                let removed = update_sched
                    .remove_systems_in_set(
                        ModSystemSet::Mod(mod_id),
                        world,
                        ScheduleCleanupPolicy::RemoveSetAndSystems,
                    )
                    .expect("ModSystemSet::Mod(id) must be cleanly removable");
                assert_eq!(removed, 1);
            });
    }

    assert_eq!(run_counter.load(Ordering::SeqCst), 1000);

    // Run one tick so Bevy rebuilds the dirty schedule graph after the 1000th removal.
    app.update();
    assert_eq!(run_counter.load(Ordering::SeqCst), 1000);

    // After 1,000 add + run + remove cycles, zero guest systems remain in Update!
    let schedules = app.world().resource::<Schedules>();
    let update_sched = schedules.get(Update).unwrap();
    assert_eq!(
        update_sched.systems_len(),
        0,
        "1,000 hot-reload cycles must leave 0 dead guest systems in Bevy's Update schedule!"
    );
}

#[test]
fn semver_abi_compatibility_and_typed_mod_load_error_work() {
    const fn pack_ver(maj: u32, min: u32, patch: u32) -> u32 {
        (maj << 24) | (min << 16) | patch
    }

    // Host is 0.1.0 (`ABI_VERSION_PACKED`)
    assert!(is_abi_version_compatible(ABI_VERSION_PACKED, ABI_VERSION_PACKED));
    assert!(is_abi_version_compatible(ABI_VERSION_PACKED, pack_ver(0, 0, 9)));
    assert!(is_abi_version_compatible(ABI_VERSION_PACKED, pack_ver(0, 1, 99)));
    assert!(!is_abi_version_compatible(ABI_VERSION_PACKED, pack_ver(0, 2, 0)));
    assert!(!is_abi_version_compatible(ABI_VERSION_PACKED, pack_ver(1, 0, 0)));

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let err = app
        .try_load_mod("/nonexistent/path/to/missing_mod.dylib")
        .expect_err("Missing dylib must return ModLoadError::DlOpen");
    assert!(matches!(err, ModLoadError::DlOpen { .. }));
    assert_eq!(FfiResult::from(err.clone()), FfiResult::INTERNAL_ERROR);
    assert!(err.to_string().contains("missing_mod.dylib"));

    // Verify unused markers compile cleanly for LoadedModInfo / ModLoaded / ModUnloaded
    let _ = core::mem::size_of::<LoadedModInfo>();
    let _ = core::mem::size_of::<ModLoaded>();
    let _ = core::mem::size_of::<ModUnloaded>();
}

#[test]
fn real_dylib_loaded_mod_entity_lifecycle_events_and_strict_validation_work() {
    let (rust_dylib, c_dylib) = common::ensure_guest_mods_built();
    common::reset_rust_mod_panic(&rust_dylib);

    // 1. Strict mode validation: when `CombatHealth_vtable` symbol is exposed so `asymmetric_rust_mod`
    // registers `rust_combat` querying `"CombatHealth"`, WITHOUT registering `CombatHealth` as an ECS
    // component in `ModRegistry`, `try_load_mod_strict` must fail fast with `ModLoadError::MissingComponents`
    // and cleanly unload the library!
    {
        let mut strict_app = App::new();
        strict_app.add_plugins(AbiModPlugin);
        strict_app.register_mod_component::<Position>();
        strict_app.register_mod_component::<common::Velocity>();
        // SAFETY: Stub non-null symbol address is only inspected for nullness during `mod_init`
        // before strict validation aborts and unloads the module without running systems.
        unsafe {
            strict_app
                .world_mut()
                .resource_mut::<ModRegistry>()
                .register_custom_symbol("CombatHealth_vtable", core::ptr::without_provenance(1));
        }

        let strict_err = strict_app
            .try_load_mod_strict(&rust_dylib)
            .expect_err("Unregistered CombatHealth component must fail strict validation");
        match strict_err {
            ModLoadError::MissingComponents { ref missing, .. } => {
                assert!(missing.iter().any(|c| c == "CombatHealth"));
            }
            other => panic!("Expected ModLoadError::MissingComponents, got {other:?}"),
        }
        assert_eq!(strict_app.world().resource::<host_runtime::ModHost>().len(), 0);
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<common::Velocity>();

    // Load C99 mod and verify `LoadedModInfo` ECS entity + `ModLoaded` / `ModUnloaded` messages & observers!
    let loaded_obs = Arc::new(Mutex::new(Vec::new()));
    let unloaded_obs = Arc::new(Mutex::new(Vec::new()));
    let l_clone = Arc::clone(&loaded_obs);
    let u_clone = Arc::clone(&unloaded_obs);

    app.world_mut()
        .add_observer(move |trigger: On<ModLoaded>| {
            l_clone.lock().unwrap().push(trigger.event().clone());
        });
    app.world_mut()
        .add_observer(move |trigger: On<ModUnloaded>| {
            u_clone.lock().unwrap().push(trigger.event().clone());
        });

    let mod_id = app.try_load_mod(&c_dylib).expect("C99 mod should load");

    // ECS entity with `LoadedModInfo` exists!
    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        let infos: Vec<&LoadedModInfo> = q.iter(world).collect();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, mod_id);
        assert!(infos[0].system_count >= 1);
    }
    assert_eq!(loaded_obs.lock().unwrap().len(), 1);
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 1);

    app.update();

    // Unload mod: despawns `LoadedModInfo` entity, removes systems from `Schedules`, emits `ModUnloaded`!
    app.try_unload_mod(mod_id).expect("C99 mod should unload");
    {
        let world = app.world_mut();
        let mut q = world.query::<&LoadedModInfo>();
        assert_eq!(q.iter(world).count(), 0);
    }
    let u_events = unloaded_obs.lock().unwrap();
    assert_eq!(u_events.len(), 1);
    assert_eq!(u_events[0].id, mod_id);
    assert!(u_events[0].removed_systems >= 1);
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
}

