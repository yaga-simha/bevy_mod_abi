//! 3D Pastel Slime Meadow simulation environment, entity spawning, and baseline host physics.

use crate::components::{ColorRgba, CuteSpirit, HomeStyle, Position, Scale3D, Velocity};
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::{Commands, Query, Res, ResMut};

pub const MEADOW_RADIUS: f32 = 8.4;
pub const DT: f32 = 1.0 / 60.0;

#[derive(Resource, Default)]
pub struct MeadowClock {
    pub tick: u64,
}

/// Curated kawaii pastel palette (strawberry milk, peach sorbet, buttercup, mint cream, sky puff, taro lavender).
const PASTEL_PALETTE: [ColorRgba; 6] = [
    ColorRgba::new(1.00, 0.64, 0.78, 1.0), // Strawberry pink
    ColorRgba::new(1.00, 0.77, 0.58, 1.0), // Peach sorbet
    ColorRgba::new(0.99, 0.93, 0.60, 1.0), // Lemon cream
    ColorRgba::new(0.62, 0.94, 0.79, 1.0), // Mint macaron
    ColorRgba::new(0.64, 0.85, 1.00, 1.0), // Baby sky blue
    ColorRgba::new(0.83, 0.71, 1.00, 1.0), // Taro lavender
];

/// Spawns cute slime spirits across the floating 3D meadow island.
pub fn spawn_slimes(mut commands: Commands, count: usize) {
    let mut seed = 0xC0FF_EE42_u32;
    let mut next_f32 = || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 8) as f32 / 16777216.0
    };

    for i in 0..count {
        // Place slimes outside the central mushroom ring [2.2 .. 7.4]
        let angle = (i as f32) * 2.399_963 + next_f32() * 0.35;
        let radius = 2.3 + next_f32() * 5.0;
        let x = radius * angle.cos();
        let z = radius * angle.sin();

        let vx = (next_f32() - 0.5) * 2.2;
        let vz = (next_f32() - 0.5) * 2.2;
        let yaw = vx.atan2(vz);

        let base_color = PASTEL_PALETTE[i % PASTEL_PALETTE.len()];
        let phase_offset = next_f32() * core::f32::consts::TAU;

        commands.spawn((
            Position::new(x, 0.0, z),
            Velocity::new(vx, 0.0, vz),
            Scale3D::uniform(1.0),
            base_color,
            CuteSpirit::new(yaw, 0.0, 0.42),
            HomeStyle {
                base_color,
                phase_offset,
            },
        ));
    }
}

/// Host physics & cozy idle animation executed during `PostUpdate`.
///
/// Guest mods execute during `Update` (mutating velocity, hop height, squash/stretch,
/// pastel colors, ear flop, and blush), while `PostUpdate` integrates horizontal motion,
/// keeps slimes safely inside the meadow island, and gently settles baseline idle breathing
/// when no mod is loaded.
pub fn host_meadow_post_physics(
    mut clock: ResMut<MeadowClock>,
    has_active_mod: Option<Res<crate::mod_manager::ModStatusResource>>,
    mut query: Query<(
        &mut Position,
        &mut Velocity,
        &mut Scale3D,
        &mut CuteSpirit,
        &HomeStyle,
    )>,
) {
    clock.tick = clock.tick.wrapping_add(1);
    let t = (clock.tick as f32) * DT;
    let baseline_idle = has_active_mod
        .as_ref()
        .map(|s| s.active_mod == crate::mod_manager::ActiveMod::None)
        .unwrap_or(true);

    for (mut pos, mut vel, mut scale, mut spirit, home) in query.iter_mut() {
        pos.x += vel.x * DT;
        pos.z += vel.z * DT;

        // Keep away from the central giant plush mushroom trunk (radius < 1.35)
        let r_sq = pos.x * pos.x + pos.z * pos.z;
        if r_sq < 1.35 * 1.35 && r_sq > 0.0001 {
            let r = r_sq.sqrt();
            let nx = pos.x / r;
            let nz = pos.z / r;
            pos.x = nx * 1.35;
            pos.z = nz * 1.35;
            let dot = vel.x * nx + vel.z * nz;
            if dot < 0.0 {
                vel.x -= 1.8 * dot * nx;
                vel.z -= 1.8 * dot * nz;
            }
        }

        // Soft circular fence reflection at the edge of the floating island
        let r_out = (pos.x * pos.x + pos.z * pos.z).sqrt();
        if r_out > MEADOW_RADIUS {
            let nx = pos.x / r_out;
            let nz = pos.z / r_out;
            pos.x = nx * MEADOW_RADIUS;
            pos.z = nz * MEADOW_RADIUS;
            let dot = vel.x * nx + vel.z * nz;
            if dot > 0.0 {
                vel.x -= 1.9 * dot * nx;
                vel.z -= 1.9 * dot * nz;
            }
        }

        if baseline_idle {
            // Cozy baseline stroll: gentle ground glide + soft jelly breathing
            pos.y *= 0.85;
            if pos.y < 0.005 {
                pos.y = 0.0;
            }
            vel.y = 0.0;

            // Keep a gentle wandering breeze speed so slimes don't stall
            let speed = (vel.x * vel.x + vel.z * vel.z).sqrt();
            if speed < 0.65 {
                let angle = home.phase_offset + t * 0.4;
                vel.x += angle.cos() * 0.04;
                vel.z += angle.sin() * 0.04;
            } else if speed > 2.2 {
                vel.x *= 0.97;
                vel.z *= 0.97;
            }

            let breath = 1.0 + 0.06 * (t * 2.8 + home.phase_offset).sin();
            let inv = 1.0 / breath.sqrt();
            scale.x += (inv - scale.x) * 0.15;
            scale.y += (breath - scale.y) * 0.15;
            scale.z += (inv - scale.z) * 0.15;

            if speed > 0.05 {
                let target_yaw = vel.x.atan2(vel.z);
                // Shortest-angle turn
                let mut diff = target_yaw - spirit.yaw;
                while diff > core::f32::consts::PI {
                    diff -= core::f32::consts::TAU;
                }
                while diff < -core::f32::consts::PI {
                    diff += core::f32::consts::TAU;
                }
                spirit.yaw += diff * 0.12;
            }
            let target_ear = 0.10 * (t * 3.4 + home.phase_offset).sin();
            spirit.ear_wiggle += (target_ear - spirit.ear_wiggle) * 0.15;
            spirit.blush += (0.42 - spirit.blush) * 0.10;
        }
    }
}
