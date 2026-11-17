//! 2D Kinetic Arena simulation environment, entity spawning, and host kinematics.

use crate::components::{ColorRgba, Position, Velocity};
use bevy_ecs::system::{Commands, Query};

/// Logical width of the particle arena.
pub const ARENA_WIDTH: f32 = 120.0;
/// Logical height of the particle arena.
pub const ARENA_HEIGHT: f32 = 60.0;
/// Fixed simulation timestep in seconds.
pub const DT: f32 = 1.0 / 60.0;
/// Wall bounce restitution factor.
pub const RESTITUTION: f32 = 0.95;

/// Spawns particle entities across the arena.
pub fn spawn_particles(mut commands: Commands, count: usize) {
    let mut seed = 0x1234_5678_u32;
    let mut next_float = || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / 16_777_216.0
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

/// Host physics integration system executed during `PostUpdate`.
///
/// Foreign mods execute in `Update`, while host kinetics, boundary clamping, and wall restitution
/// execute in `PostUpdate`.
pub fn host_physics_integration(mut query: Query<(&mut Position, &mut Velocity)>) {
    for (mut pos, mut vel) in &mut query {
        pos.x += vel.x * DT;
        pos.y += vel.y * DT;

        if pos.x <= 1.0 {
            pos.x = 1.0;
            vel.x = -vel.x * RESTITUTION;
        } else if pos.x >= ARENA_WIDTH - 1.0 {
            pos.x = ARENA_WIDTH - 1.0;
            vel.x = -vel.x * RESTITUTION;
        }

        if pos.y <= 1.0 {
            pos.y = 1.0;
            vel.y = -vel.y * RESTITUTION;
        } else if pos.y >= ARENA_HEIGHT - 1.0 {
            pos.y = ARENA_HEIGHT - 1.0;
            vel.y = -vel.y * RESTITUTION;
        }

        vel.x *= 0.999;
        vel.y *= 0.999;
    }
}
