//! 3D Pastel Slime Meadow simulation environment, entity spawning, and baseline host physics.

use crate::components::{ColorRgba, CuteSpirit, HomeStyle, Position, Scale3D, Velocity};
use avian3d::prelude::{Collider, CollidingEntities, Friction, LockedAxes, Restitution, RigidBody};
use avian3d::prelude::{LinearVelocity as AvianVel, Position as AvianPos};
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::{Commands, Query, Res, ResMut};
use bevy_math::Vec3;
use bevy_transform::components::Transform;

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

/// Spawns static colliders for the floating meadow island and giant plush mushroom.
pub fn spawn_meadow_colliders(mut commands: Commands) {
    // 1. Centerpiece giant plush mushroom trunk collider
    commands.spawn((
        RigidBody::Static,
        Collider::cylinder(1.35, 3.0),
        Restitution::new(0.85),
        Friction::new(0.25),
        AvianPos::from_xyz(0.0, 1.0, 0.0),
        Transform::from_xyz(0.0, 1.0, 0.0),
    ));

    // 2. Island grass foundation floor collider
    commands.spawn((
        RigidBody::Static,
        Collider::cylinder(MEADOW_RADIUS + 0.5, 1.0),
        Restitution::new(0.70),
        Friction::new(0.30),
        AvianPos::from_xyz(0.0, -0.5, 0.0),
        Transform::from_xyz(0.0, -0.5, 0.0),
    ));
}

/// Spawns cute slime spirits across the floating 3D meadow island.
pub fn spawn_slimes(mut commands: Commands, count: usize) {
    spawn_slimes_batch(&mut commands, count, 0xC0FF_EE42_u32);
}

/// Spawns a single cute bunny-slime with complete C-ABI and Avian 3D physics components.
pub fn spawn_single_slime(
    commands: &mut Commands,
    pos: Position,
    vel: Velocity,
    scale: Scale3D,
    color: ColorRgba,
    spirit: CuteSpirit,
    home: HomeStyle,
) -> bevy_ecs::entity::Entity {
    commands
        .spawn((
            pos,
            vel,
            scale,
            color,
            spirit,
            home,
            // Avian 3D ECS Physics with simpler Capsule collider
            RigidBody::Dynamic,
            Collider::capsule(0.42, 0.36),
            CollidingEntities::default(),
            Restitution::new(0.88),
            Friction::new(0.20),
            LockedAxes::ROTATION_LOCKED,
            AvianPos::from_xyz(pos.x, pos.y, pos.z),
            AvianVel(Vec3::new(vel.x, vel.y, vel.z)),
            Transform::from_xyz(pos.x, pos.y, pos.z),
        ))
        .id()
}

/// Spawns a batch of cute slime spirits with a specified pseudorandom seed.
pub fn spawn_slimes_batch(commands: &mut Commands, count: usize, seed_start: u32) {
    let mut seed = seed_start;
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

        let base_color = PASTEL_PALETTE[(i + (seed_start as usize)) % PASTEL_PALETTE.len()];
        let phase_offset = next_f32() * core::f32::consts::TAU;

        spawn_single_slime(
            commands,
            Position::new(x, 0.0, z),
            Velocity::new(vx, 0.0, vz),
            Scale3D::uniform(1.0),
            base_color,
            CuteSpirit::new(yaw, 0.0, 0.42),
            HomeStyle {
                base_color,
                phase_offset,
            },
        );
    }
}

type SlimeQueryItem<'a> = (
    &'a mut Position,
    &'a mut Velocity,
    &'a mut Scale3D,
    &'a mut CuteSpirit,
    &'a HomeStyle,
    &'a mut AvianPos,
    &'a mut AvianVel,
    &'a CollidingEntities,
);

/// Host physics, Avian collider synchronization, and cozy idle animation executed during `PostUpdate`.
#[allow(clippy::type_complexity)]
pub fn host_meadow_post_physics(
    mut clock: ResMut<MeadowClock>,
    has_active_mod: Option<Res<crate::mod_manager::ModStatusResource>>,
    mut query: Query<SlimeQueryItem>,
) {
    clock.tick = clock.tick.wrapping_add(1);
    let t = (clock.tick as f32) * DT;
    let has_mod = has_active_mod
        .as_ref()
        .map(|s| s.active_mod != crate::mod_manager::ActiveMod::None)
        .unwrap_or(false);

    for (mut pos, mut vel, mut scale, mut spirit, home, mut avian_pos, mut avian_vel, colliding) in
        query.iter_mut()
    {
        if has_mod {
            // Mod updated C-ABI components during Update; sync to Avian physics
            avian_pos.0 = Vec3::new(pos.x, pos.y, pos.z);
            avian_vel.0 = Vec3::new(vel.x, vel.y, vel.z);
        } else {
            // Baseline stroll: integrate horizontal motion and idle wandering
            pos.x += vel.x * DT;
            pos.z += vel.z * DT;
            avian_pos.0.x = pos.x;
            avian_pos.0.z = pos.z;

            pos.y *= 0.85;
            if pos.y < 0.005 {
                pos.y = 0.0;
            }
            vel.y = 0.0;
            avian_pos.0.y = pos.y;
            avian_vel.0.y = 0.0;

            // Gentle wandering breeze speed so slimes don't stall
            let speed = (vel.x * vel.x + vel.z * vel.z).sqrt();
            if speed < 0.65 {
                let angle = home.phase_offset + t * 0.4;
                vel.x += angle.cos() * 0.04;
                vel.z += angle.sin() * 0.04;
                avian_vel.0.x = vel.x;
                avian_vel.0.z = vel.z;
            } else if speed > 2.2 {
                vel.x *= 0.97;
                vel.z *= 0.97;
                avian_vel.0.x = vel.x;
                avian_vel.0.z = vel.z;
            }

            let breath = 1.0 + 0.06 * (t * 2.8 + home.phase_offset).sin();
            let inv = 1.0 / breath.sqrt();
            scale.x += (inv - scale.x) * 0.15;
            scale.y += (breath - scale.y) * 0.15;
            scale.z += (inv - scale.z) * 0.15;

            if speed > 0.05 {
                let target_yaw = vel.x.atan2(vel.z);
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

        // Keep away from the central giant plush mushroom trunk (radius < 1.35)
        let r_sq = pos.x * pos.x + pos.z * pos.z;
        if r_sq < 1.35 * 1.35 && r_sq > 0.0001 {
            let r = r_sq.sqrt();
            let nx = pos.x / r;
            let nz = pos.z / r;
            pos.x = nx * 1.35;
            pos.z = nz * 1.35;
            avian_pos.0.x = pos.x;
            avian_pos.0.z = pos.z;
            let dot = vel.x * nx + vel.z * nz;
            if dot < 0.0 {
                vel.x -= 1.8 * dot * nx;
                vel.z -= 1.8 * dot * nz;
                avian_vel.0.x = vel.x;
                avian_vel.0.z = vel.z;
            }
        }

        // Soft circular fence reflection at the edge of the floating island
        let r_out = (pos.x * pos.x + pos.z * pos.z).sqrt();
        if r_out > MEADOW_RADIUS {
            let nx = pos.x / r_out;
            let nz = pos.z / r_out;
            pos.x = nx * MEADOW_RADIUS;
            pos.z = nz * MEADOW_RADIUS;
            avian_pos.0.x = pos.x;
            avian_pos.0.z = pos.z;
            let dot = vel.x * nx + vel.z * nz;
            if dot > 0.0 {
                vel.x -= 1.9 * dot * nx;
                vel.z -= 1.9 * dot * nz;
                avian_vel.0.x = vel.x;
                avian_vel.0.z = vel.z;
            }
        }

        // Avian contact manifold collision reaction
        if !colliding.is_empty() {
            spirit.blush = 1.0;
            spirit.ear_wiggle = (spirit.ear_wiggle + 0.35).clamp(-0.6, 0.6);
            scale.y = (scale.y * 0.88).clamp(0.68, 1.35);
            scale.x = 1.0 / scale.y.sqrt();
            scale.z = scale.x;
        }
    }

    // Slime-to-slime pairwise collisions and bouncy elastic jelly reaction
    let mut combinations = query.iter_combinations_mut();
    while let Some([mut a, mut b]) = combinations.fetch_next() {
        let (pos_a, vel_a, scale_a, spirit_a, _, avian_pos_a, avian_vel_a, _) = &mut a;
        let (pos_b, vel_b, scale_b, spirit_b, _, avian_pos_b, avian_vel_b, _) = &mut b;

        let dx = pos_a.x - pos_b.x;
        let dz = pos_a.z - pos_b.z;
        let dist_sq = dx * dx + dz * dz;

        // Collision radius takes into account horizontal squash-stretch
        let r_a = 0.46 * scale_a.x.max(0.6);
        let r_b = 0.46 * scale_b.x.max(0.6);
        let min_dist = r_a + r_b;
        let min_dist_sq = min_dist * min_dist;

        // Vertical overlap check (slimes can leap over each other if dy is high)
        let dy = (pos_a.y - pos_b.y).abs();
        let max_dy = 0.70 * (scale_a.y + scale_b.y);

        if dist_sq < min_dist_sq && dy < max_dy {
            let dist = dist_sq.sqrt();
            let (nx, nz) = if dist > 0.0001 {
                (dx / dist, dz / dist)
            } else {
                (1.0, 0.0)
            };

            // 1. Positional soft separation so slimes don't overlap/clip
            let overlap = min_dist - dist;
            let sep = overlap * 0.52;
            pos_a.x += nx * sep;
            pos_a.z += nz * sep;
            pos_b.x -= nx * sep;
            pos_b.z -= nz * sep;
            avian_pos_a.0.x = pos_a.x;
            avian_pos_a.0.z = pos_a.z;
            avian_pos_b.0.x = pos_b.x;
            avian_pos_b.0.z = pos_b.z;

            // 2. Elastic jelly bounce & momentum exchange
            let rel_vx = vel_a.x - vel_b.x;
            let rel_vz = vel_a.z - vel_b.z;
            let vn = rel_vx * nx + rel_vz * nz;

            if vn < 0.0 {
                // Moving toward each other: bounce apart!
                let restitution = 0.88;
                let pop_boost = 0.75; // cute boing impulse
                let impulse = -(1.0 + restitution) * vn + pop_boost;

                vel_a.x += nx * impulse * 0.5;
                vel_a.z += nz * impulse * 0.5;
                vel_b.x -= nx * impulse * 0.5;
                vel_b.z -= nz * impulse * 0.5;
            } else {
                // Already parting: subtle repulsive nudge
                vel_a.x += nx * 0.20;
                vel_a.z += nz * 0.20;
                vel_b.x -= nx * 0.20;
                vel_b.z -= nz * 0.20;
            }
            avian_vel_a.0.x = vel_a.x;
            avian_vel_a.0.z = vel_a.z;
            avian_vel_b.0.x = vel_b.x;
            avian_vel_b.0.z = vel_b.z;

            // 3. Cute vertical boing hop & squish reaction
            if pos_a.y < 0.25 && vel_a.y < 0.8 {
                vel_a.y = (vel_a.y + 1.2).min(3.5);
                avian_vel_a.0.y = vel_a.y;
            }
            if pos_b.y < 0.25 && vel_b.y < 0.8 {
                vel_b.y = (vel_b.y + 1.2).min(3.5);
                avian_vel_b.0.y = vel_b.y;
            }

            // Impact jelly squish
            scale_a.y = (scale_a.y * 0.88).clamp(0.68, 1.35);
            scale_b.y = (scale_b.y * 0.88).clamp(0.68, 1.35);

            // 4. Emotional reaction: bright rosy cheeks & startled ear wobble!
            spirit_a.blush = 1.0;
            spirit_b.blush = 1.0;
            spirit_a.ear_wiggle = (spirit_a.ear_wiggle + 0.35).clamp(-0.6, 0.6);
            spirit_b.ear_wiggle = (spirit_b.ear_wiggle - 0.35).clamp(-0.6, 0.6);

            // Keep slimes within the meadow island after collision separation
            clamp_island_bounds(pos_a, vel_a);
            clamp_island_bounds(pos_b, vel_b);
            avian_pos_a.0.x = pos_a.x;
            avian_pos_a.0.z = pos_a.z;
            avian_pos_b.0.x = pos_b.x;
            avian_pos_b.0.z = pos_b.z;
        }
    }
}

#[inline]
fn clamp_island_bounds(pos: &mut Position, vel: &mut Velocity) {
    let r_out = (pos.x * pos.x + pos.z * pos.z).sqrt();
    if r_out > MEADOW_RADIUS {
        let nx = pos.x / r_out;
        let nz = pos.z / r_out;
        pos.x = nx * MEADOW_RADIUS;
        pos.z = nz * MEADOW_RADIUS;
        let dot = vel.x * nx + vel.z * nz;
        if dot > 0.0 {
            vel.x -= 1.8 * dot * nx;
            vel.z -= 1.8 * dot * nz;
        }
    }
    let r_in_sq = pos.x * pos.x + pos.z * pos.z;
    if r_in_sq < 1.35 * 1.35 && r_in_sq > 0.0001 {
        let r = r_in_sq.sqrt();
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
}
