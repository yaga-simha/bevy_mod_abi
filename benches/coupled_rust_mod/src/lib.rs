#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::type_complexity)]

//! Coupled Rust `dylib` Mod (`bevy_dynamic_plugin` style).
//!
//! Shares unstable Rust ABI, `TypeId`s, heap allocator, and `bevy_ecs::system::Query` directly
//! with the host binary via `shared_scenario` (`crate-type = ["dylib"]`).
//! Demonstrates:
//! 1. Near-native steady-state execution speed (direct struct offsets, no C-ABI VTable calls).
//! 2. OS dynamic-linker (`dlopen`) overhead of resolving hundreds of mangled Rust/Bevy symbols.
//! 3. Strict compiler & profile lock-in (cannot load a `debug` Rust `dylib` into a `release` Host!).

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use shared_scenario::bevy_app::{App, Update};
use shared_scenario::bevy_ecs::query::{With, Without};
use shared_scenario::bevy_ecs::system::{Local, Query};
use shared_scenario::{
    AuraEmitter, CachedNativeEmitter, CombatUnitTag, EmitterBeaconTag, Faction, FrozenFlag,
    Kinematics, Resistance, Transform3D, Vitality, SIM_DT,
};

static ACTIVE: AtomicBool = AtomicBool::new(true);
static COUPLED_VARIANT: AtomicU32 = AtomicU32::new(0);

/// Selects `0 = Direct Nested Query`, `1 = Pre-Cached SoA + Query`.
#[no_mangle]
pub extern "C" fn coupled_rust_mod_set_variant(variant: u32) {
    COUPLED_VARIANT.store(variant, Ordering::Release);
}

pub fn coupled_nested_aura_combat_system(
    mut units: Query<
        (
            &mut Transform3D,
            &mut Kinematics,
            &mut Vitality,
            &Resistance,
            &Faction,
        ),
        (
            With<CombatUnitTag>,
            Without<EmitterBeaconTag>,
            Without<FrozenFlag>,
        ),
    >,
    emitters: Query<
        (&Transform3D, &AuraEmitter, &Faction),
        (
            With<EmitterBeaconTag>,
            Without<CombatUnitTag>,
            Without<FrozenFlag>,
        ),
    >,
) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }

    for (mut u_tf, mut u_kin, mut u_vit, u_res, u_fac) in &mut units {
        let ux = u_tf.position.x;
        let uy = u_tf.position.y;
        let uz = u_tf.position.z;

        let mut vx = u_kin.velocity.x;
        let mut vy = u_kin.velocity.y;
        let mut vz = u_kin.velocity.z;

        let mut health = u_vit.health;
        let max_health = u_vit.max_health;
        let mut shield = u_vit.shield;
        let mut energy = u_vit.energy;

        for (e_tf, aura, e_fac) in &emitters {
            let dx = ux - e_tf.position.x;
            let dy = uy - e_tf.position.y;
            let dz = uz - e_tf.position.z;
            let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;
            let rad_sq = aura.radius * aura.radius;

            if dist_sq < rad_sq {
                let dist = dist_sq.sqrt();
                let inv_dist = 1.0_f32 / dist;
                let norm = 1.0_f32 - (dist / aura.radius);
                let atten = norm / (1.0_f32 + aura.falloff * dist_sq);

                if u_fac.team_id == e_fac.team_id {
                    let boost = aura.force * 0.25_f32 * atten * SIM_DT;
                    vx += e_tf.forward.x * boost;
                    vy += e_tf.forward.y * boost;
                    vz += e_tf.forward.z * boost;

                    let heal = aura.heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * u_res.ionic);
                    let nh = health + heal;
                    health = if nh > max_health { max_health } else { nh };

                    let ne = energy + 2.5_f32 * atten * SIM_DT;
                    energy = if ne > 100.0_f32 { 100.0_f32 } else { ne };
                } else {
                    let imp = aura.force
                        * atten
                        * SIM_DT
                        * (1.0_f32 - 0.5_f32 * u_res.kinetic)
                        * inv_dist;
                    vx += dx * imp;
                    vy += dy * imp;
                    vz += dz * imp;

                    let raw_dmg =
                        aura.thermal_dps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * u_res.thermal);
                    let absorbed = if shield >= raw_dmg { raw_dmg } else { shield };
                    shield -= absorbed;
                    let nh = health - (raw_dmg - absorbed);
                    health = if nh < 0.0_f32 { 0.0_f32 } else { nh };

                    let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * u_res.ionic);
                    let ne = energy - drain;
                    energy = if ne < 0.0_f32 { 0.0_f32 } else { ne };
                }
            }
        }

        let damp = 1.0_f32 / (1.0_f32 + u_kin.drag * SIM_DT);
        vx *= damp;
        vy *= damp;
        vz *= damp;

        let speed_sq = vx * vx + vy * vy + vz * vz;
        let max_sq = u_kin.max_speed * u_kin.max_speed;
        if speed_sq > max_sq && speed_sq > 1.0e-12_f32 {
            let scale = u_kin.max_speed / speed_sq.sqrt();
            vx *= scale;
            vy *= scale;
            vz *= scale;
        }

        u_kin.velocity.x = vx;
        u_kin.velocity.y = vy;
        u_kin.velocity.z = vz;

        u_tf.position.x = ux + vx * SIM_DT;
        u_tf.position.y = uy + vy * SIM_DT;
        u_tf.position.z = uz + vz * SIM_DT;

        let final_speed_sq = vx * vx + vy * vy + vz * vz;
        if final_speed_sq > 1.0e-6_f32 {
            let inv_s = 1.0_f32 / final_speed_sq.sqrt();
            u_tf.forward.x = vx * inv_s;
            u_tf.forward.y = vy * inv_s;
            u_tf.forward.z = vz * inv_s;
        }

        u_vit.health = health;
        u_vit.shield = shield;
        u_vit.energy = energy;
    }
}

pub fn coupled_cached_soa_nested_aura_combat_system(
    mut cached_emitters: Local<Vec<CachedNativeEmitter>>,
    mut units: Query<
        (
            &mut Transform3D,
            &mut Kinematics,
            &mut Vitality,
            &Resistance,
            &Faction,
        ),
        (
            With<CombatUnitTag>,
            Without<EmitterBeaconTag>,
            Without<FrozenFlag>,
        ),
    >,
    emitters: Query<
        (&Transform3D, &AuraEmitter, &Faction),
        (
            With<EmitterBeaconTag>,
            Without<CombatUnitTag>,
            Without<FrozenFlag>,
        ),
    >,
) {
    if !ACTIVE.load(Ordering::Acquire) {
        return;
    }

    cached_emitters.clear();
    for (e_tf, aura, e_fac) in &emitters {
        cached_emitters.push(CachedNativeEmitter {
            px: e_tf.position.x,
            py: e_tf.position.y,
            pz: e_tf.position.z,
            fx: e_tf.forward.x,
            fy: e_tf.forward.y,
            fz: e_tf.forward.z,
            radius: aura.radius,
            rad_sq: aura.radius * aura.radius,
            force: aura.force,
            thermal_dps: aura.thermal_dps,
            heal_hps: aura.heal_hps,
            falloff: aura.falloff,
            team_id: e_fac.team_id,
        });
    }

    let em_slice = cached_emitters.as_slice();
    for (mut u_tf, mut u_kin, mut u_vit, u_res, u_fac) in &mut units {
        let ux = u_tf.position.x;
        let uy = u_tf.position.y;
        let uz = u_tf.position.z;

        let mut vx = u_kin.velocity.x;
        let mut vy = u_kin.velocity.y;
        let mut vz = u_kin.velocity.z;

        let mut health = u_vit.health;
        let max_health = u_vit.max_health;
        let mut shield = u_vit.shield;
        let mut energy = u_vit.energy;

        for em in em_slice {
            let dx = ux - em.px;
            let dy = uy - em.py;
            let dz = uz - em.pz;
            let dist_sq = dx * dx + dy * dy + dz * dz + 0.25_f32;

            if dist_sq < em.rad_sq {
                let dist = dist_sq.sqrt();
                let inv_dist = 1.0_f32 / dist;
                let norm = 1.0_f32 - (dist / em.radius);
                let atten = norm / (1.0_f32 + em.falloff * dist_sq);

                if u_fac.team_id == em.team_id {
                    let boost = em.force * 0.25_f32 * atten * SIM_DT;
                    vx += em.fx * boost;
                    vy += em.fy * boost;
                    vz += em.fz * boost;

                    let heal = em.heal_hps * atten * SIM_DT * (1.0_f32 + 0.1_f32 * u_res.ionic);
                    let nh = health + heal;
                    health = if nh > max_health { max_health } else { nh };

                    let ne = energy + 2.5_f32 * atten * SIM_DT;
                    energy = if ne > 100.0_f32 { 100.0_f32 } else { ne };
                } else {
                    let imp =
                        em.force * atten * SIM_DT * (1.0_f32 - 0.5_f32 * u_res.kinetic) * inv_dist;
                    vx += dx * imp;
                    vy += dy * imp;
                    vz += dz * imp;

                    let raw_dmg =
                        em.thermal_dps * atten * SIM_DT * (1.0_f32 - 0.6_f32 * u_res.thermal);
                    let absorbed = if shield >= raw_dmg { raw_dmg } else { shield };
                    shield -= absorbed;
                    let nh = health - (raw_dmg - absorbed);
                    health = if nh < 0.0_f32 { 0.0_f32 } else { nh };

                    let drain = 1.5_f32 * atten * SIM_DT * (1.0_f32 - 0.4_f32 * u_res.ionic);
                    let ne = energy - drain;
                    energy = if ne < 0.0_f32 { 0.0_f32 } else { ne };
                }
            }
        }

        let damp = 1.0_f32 / (1.0_f32 + u_kin.drag * SIM_DT);
        vx *= damp;
        vy *= damp;
        vz *= damp;

        let speed_sq = vx * vx + vy * vy + vz * vz;
        let max_sq = u_kin.max_speed * u_kin.max_speed;
        if speed_sq > max_sq && speed_sq > 1.0e-12_f32 {
            let scale = u_kin.max_speed / speed_sq.sqrt();
            vx *= scale;
            vy *= scale;
            vz *= scale;
        }

        u_kin.velocity.x = vx;
        u_kin.velocity.y = vy;
        u_kin.velocity.z = vz;

        u_tf.position.x = ux + vx * SIM_DT;
        u_tf.position.y = uy + vy * SIM_DT;
        u_tf.position.z = uz + vz * SIM_DT;

        let final_speed_sq = vx * vx + vy * vy + vz * vz;
        if final_speed_sq > 1.0e-6_f32 {
            let inv_s = 1.0_f32 / final_speed_sq.sqrt();
            u_tf.forward.x = vx * inv_s;
            u_tf.forward.y = vy * inv_s;
            u_tf.forward.z = vz * inv_s;
        }

        u_vit.health = health;
        u_vit.shield = shield;
        u_vit.energy = energy;
    }
}

/// Entry point using unstable Rust ABI (`&mut App`).
#[no_mangle]
#[allow(improper_ctypes_definitions)]
pub extern "Rust" fn coupled_rust_mod_register(app: &mut App) {
    ACTIVE.store(true, Ordering::Release);
    if COUPLED_VARIANT.load(Ordering::Acquire) == 1 {
        app.add_systems(Update, coupled_cached_soa_nested_aura_combat_system);
    } else {
        app.add_systems(Update, coupled_nested_aura_combat_system);
    }
}

#[no_mangle]
pub extern "C" fn coupled_rust_mod_shutdown() {
    ACTIVE.store(false, Ordering::Release);
}
