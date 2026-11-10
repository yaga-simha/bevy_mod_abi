//! 2D Kinetic Arena simulation environment, entity spawning, and kinematics.

use crate::components::{ColorRgba, Position, Velocity};
use bevy_ecs::system::{Commands, Query};

pub const ARENA_WIDTH: f32 = 120.0;
pub const ARENA_HEIGHT: f32 = 60.0;
pub const DT: f32 = 1.0 / 60.0;
pub const RESTITUTION: f32 = 0.95;

/// Spawns particle entities across the arena.
pub fn spawn_particles(mut commands: Commands, count: usize) {
    // Deterministic pseudo-random generation to avoid external rand dependency
    let mut seed = 0x1234_5678_u32;
    let mut next_float = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 16777216.0
    };

    for _ in 0..count {
        let x = 10.0 + next_float() * (ARENA_WIDTH - 20.0);
        let y = 10.0 + next_float() * (ARENA_HEIGHT - 20.0);
        let vx = (next_float() - 0.5) * 20.0;
        let vy = (next_float() - 0.5) * 20.0;
        let color = if next_float() > 0.5 {
            ColorRgba::cyan()
        } else {
            ColorRgba::white()
        };

        commands.spawn((Position::new(x, y, 0.0), Velocity::new(vx, vy, 0.0), color));
    }
}

/// Host physics integration system executed during PostUpdate.
///
/// Ensures strict schedule phasing: foreign mods execute in Update,
/// while host kinetics, boundary clamping, and wall restitution execute here,
/// eliminating borrow conflicts.
pub fn host_physics_integration(mut query: Query<(&mut Position, &mut Velocity)>) {
    for (mut pos, mut vel) in query.iter_mut() {
        pos.x += vel.x * DT;
        pos.y += vel.y * DT;

        // X wall reflections
        if pos.x <= 1.0 {
            pos.x = 1.0;
            vel.x = -vel.x * RESTITUTION;
        } else if pos.x >= ARENA_WIDTH - 1.0 {
            pos.x = ARENA_WIDTH - 1.0;
            vel.x = -vel.x * RESTITUTION;
        }

        // Y wall reflections
        if pos.y <= 1.0 {
            pos.y = 1.0;
            vel.y = -vel.y * RESTITUTION;
        } else if pos.y >= ARENA_HEIGHT - 1.0 {
            pos.y = ARENA_HEIGHT - 1.0;
            vel.y = -vel.y * RESTITUTION;
        }

        // Subtle air drag to prevent kinetic explosion
        vel.x *= 0.999;
        vel.y *= 0.999;
    }
}
