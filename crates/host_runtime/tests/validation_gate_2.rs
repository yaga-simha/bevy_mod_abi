#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{ResMut, Resource};
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::Query;
use core::ffi::c_void;
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_column_stride, host_iter_entities, host_iter_next_chunk,
    AbiModPlugin, DynamicCSystem, ModAppExt, ModRegistry,
};
use mod_derive::ModExposable;
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

struct Gate2UserData {
    total_processed: usize,
    first_despawned: FfiEntity,
    spawned_entity: FfiEntity,
}

/// Simulated guest mod C-ABI callback satisfying Validation Gate 2:
/// - Verifies exact byte strides matching `core::mem::size_of::<T>()`.
/// - Traverses table columns via SoA chunk iteration across 10,000 entities.
/// - Mutates Position with Velocity byte strides, triggering Changed<Position>.
/// - Enqueues deferred entity spawn, component insertion, and entity despawn.
unsafe extern "C" fn validation_gate_2_guest_system(
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

        if pos_ptr.is_null() || vel_ptr.is_null() || tag_ptr.is_null() || entities_ptr.is_null() {
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
        // SAFETY: user_data points to a valid Gate2UserData struct.
        unsafe {
            let data = &mut *(user_data as *mut Gate2UserData);
            data.total_processed = total_processed;
            data.first_despawned = first_entity_to_despawn;
            data.spawned_entity = spawned_entity;
        }
    }

    FfiResult::OK
}

#[test]
fn test_validation_gate_2_host_runtime_10k_entities() {
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
        host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::READ,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "NameTag".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let mut user_data = Gate2UserData {
        total_processed: 0,
        first_despawned: FfiEntity(0),
        spawned_entity: FfiEntity(0),
    };

    let dynamic_sys = DynamicCSystem::new(
        "gate_2_mutator".to_string(),
        query_descs,
        validation_gate_2_guest_system,
        &mut user_data as *mut Gate2UserData as *mut c_void,
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
