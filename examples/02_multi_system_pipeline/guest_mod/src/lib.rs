//! Example 2: Multi-System Pipeline Guest Mod
//!
//! Demonstrates 3 interacting systems, all 4 query filter modes (READ, WRITE,
//! FILTER_ONLY, WITHOUT), safe multi-column disjoint borrows (`read_write`,
//! `entities_and_column_mut`), deferred commands (`spawn_empty`, `insert_component`,
//! `despawn`), and automatic leak-free `GuestApp` state cleanup on unload.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use guest_sdk::prelude::*;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

// SAFETY: `Position` is `#[repr(C)]` POD (`3 * f32`) matching host `components::Position`.
unsafe impl GuestComponent for Position {
    const NAME: &'static core::ffi::CStr = c"Position";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

// SAFETY: `Velocity` is `#[repr(C)]` POD (`3 * f32`) matching host `components::Velocity`.
unsafe impl GuestComponent for Velocity {
    const NAME: &'static core::ffi::CStr = c"Velocity";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

// SAFETY: `Health` is `#[repr(C)]` POD (`2 * f32`) matching host `components::Health`.
unsafe impl GuestComponent for Health {
    const NAME: &'static core::ffi::CStr = c"Health";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PlayerTag {
    pub id: u32,
}

// SAFETY: `PlayerTag` is `#[repr(C)]` POD (`u32`) matching host `components::PlayerTag`.
unsafe impl GuestComponent for PlayerTag {
    const NAME: &'static core::ffi::CStr = c"PlayerTag";
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct DroneTag {
    pub squad: u32,
}

// SAFETY: `DroneTag` is `#[repr(C)]` POD (`u32`) matching host `components::DroneTag`.
unsafe impl GuestComponent for DroneTag {
    const NAME: &'static core::ffi::CStr = c"DroneTag";
}

#[derive(Default)]
struct PipelineState {
    tick_counter: u32,
    reinforcements_spawned: u32,
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    if app.resolve_symbol(c"Position_vtable").is_null()
        || app.resolve_symbol(c"Velocity_vtable").is_null()
        || app.resolve_symbol(c"Health_vtable").is_null()
    {
        return FfiResult::INVALID_COMPONENT;
    }

    // 1. KinematicsMovementSystem: Position (WRITE, col 0), Velocity (READ, col 1)
    //    Uses disjoint `chunk.read_write::<Velocity, Position>(1, 0)` in 100% safe Rust!
    let q1 = [Position::write_desc(), Velocity::read_desc()];
    let res1 = app.add_system(c"KinematicsMovementSystem", &q1, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            let Some((vel, pos)) = chunk.read_write::<Velocity, Position>(1, 0) else {
                return FfiResult::INTERNAL_ERROR;
            };
            for (p, v) in pos.iter_mut().zip(vel.iter()) {
                p.x += v.x * 0.1;
                p.y += v.y * 0.1;
            }
        }
        FfiResult::OK
    });
    if res1.is_err() {
        return res1;
    }

    // 2. FleetLifecycleSystem: Health (WRITE, col 0), Position (READ, col 1), PlayerTag (WITHOUT, col 2)
    //    Captures `PipelineState` inside the closure — freed automatically on `bevy_abi_mod_shutdown`!
    let mut state = PipelineState::default();
    let q2 = [
        Health::write_desc(),
        Position::read_desc(),
        PlayerTag::without_desc(),
    ];
    let res2 = app.add_system(c"FleetLifecycleSystem", &q2, move |ctx| {
        state.tick_counter += 1;
        let (iter, queue) = ctx.iter_and_commands();

        while let Some(mut chunk) = iter.next_chunk_view() {
            let Some((entities, healths)) = chunk.entities_and_column_mut::<Health>(0) else {
                return FfiResult::INTERNAL_ERROR;
            };
            for (&entity, health) in entities.iter().zip(healths.iter_mut()) {
                let new_health = health.current - 10.0;
                if new_health <= 0.0 {
                    queue.despawn(entity);
                } else {
                    health.current = new_health;
                }
            }
        }

        // Reinforcement Spawner Logic: spawn Drone Gamma on tick 1 with 50 HP
        if state.tick_counter == 1 && state.reinforcements_spawned == 0 {
            let drone = queue.spawn_empty();
            if drone.0 == 0 {
                return FfiResult::INTERNAL_ERROR;
            }
            queue.insert_component(drone, &Position { x: 30.0, y: 0.0, z: 0.0 });
            queue.insert_component(drone, &Velocity { x: 2.0, y: 1.0, z: 0.0 });
            queue.insert_component(drone, &Health { current: 50.0, max: 50.0 });
            queue.insert_component(drone, &DroneTag { squad: 2 });
            state.reinforcements_spawned += 1;
        }

        FfiResult::OK
    });
    if res2.is_err() {
        return res2;
    }

    // 3. PlayerRegenSystem: Health (WRITE, col 0), PlayerTag (FILTER_ONLY, col 1)
    let q3 = [Health::write_desc(), PlayerTag::with_desc()];
    app.add_system(c"PlayerRegenSystem", &q3, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            let Some(healths) = chunk.column_mut::<Health>(0) else {
                return FfiResult::INTERNAL_ERROR;
            };
            for health in healths {
                health.current = (health.current + 5.0).min(health.max);
            }
        }
        FfiResult::OK
    })
}

export_mod!(setup);
