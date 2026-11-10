#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Shared Complex Multi-Archetype Nested Query Benchmark Scenario.
//!
//! Compiled as a shared Rust `dylib` so that:
//! 1. `permutation_bench` (host binary) runs Pure Native Bevy (no-mod) baselines and C-ABI mods.
//! 2. `coupled_rust_mod` (coupled Rust `dylib` plugin) dynamically links against the exact same
//!    `bevy_ecs`, `bevy_app`, and component `TypeId` definitions in memory — modeling unstable
//!    Rust-ABI dynamic plugins (`bevy_dynamic_plugin` style).

pub use bevy_app;
pub use bevy_ecs;
pub use host_runtime;

use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::query::{With, Without};
use bevy_ecs::system::Query;
use bevy_ecs::world::World;
use host_runtime::ModAppExt;
use mod_derive::ModExposable;

pub const SIM_DT: f32 = 0.016_f32;

// ============================================================================
// 1. Nested Compound Structs & ECS Components (`#[derive(ModExposable)]`)
// ============================================================================

/// Nested 3D vector exposed via interior pointers (`<Struct>_ptr_<field>`) and `Vec3fVTable`.
#[derive(Debug, Clone, Copy, PartialEq, Default, ModExposable)]
pub struct Vec3f {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// 3D spatial transform containing TWO nested `Vec3f` structs (`position` and `forward`).
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Transform3D {
    pub position: Vec3f,
    pub forward: Vec3f,
    pub scale: f32,
}

/// Linear kinematics containing a nested `Vec3f` velocity, drag coefficient, and speed cap.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Kinematics {
    pub velocity: Vec3f,
    pub drag: f32,
    pub max_speed: f32,
}

/// Multi-pool combat vitality state mutated by friendly/hostile aura interactions.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Vitality {
    pub health: f32,
    pub max_health: f32,
    pub shield: f32,
    pub energy: f32,
}

/// Damage/impulse mitigation resistances.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Resistance {
    pub kinetic: f32,
    pub thermal: f32,
    pub ionic: f32,
}

/// Spatial field emitter parameters read by the inner nested query.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct AuraEmitter {
    pub radius: f32,
    pub force: f32,
    pub thermal_dps: f32,
    pub heal_hps: f32,
    pub falloff: f32,
}

/// Faction alignment (`team_id`) and deterministic entity serial ID (`serial_id`).
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Faction {
    pub team_id: u32,
    pub serial_id: u32,
}

// --- Archetype Filter & Fragmentation Components ---

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct EmitterBeaconTag {
    pub channel: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct CombatUnitTag {
    pub unit_class: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct FrozenFlag {
    pub remaining_ticks: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct ArmorPlating {
    pub thickness: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct JetpackModule {
    pub fuel: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct PsionicWard {
    pub resonance: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct SquadBanner {
    pub squad_id: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct StealthCloak {
    pub opacity: f32,
}

#[derive(Component, Debug, Clone, PartialEq, ModExposable)]
pub struct TelemetryTag {
    pub label: String,
}

/// Registers all 15 components and the nested `Vec3f` compound type into `App` and `ModRegistry`.
pub fn register_all_scenario_components(app: &mut App) {
    app.register_mod_type::<Vec3f>();
    app.register_mod_component::<Transform3D>();
    app.register_mod_component::<Kinematics>();
    app.register_mod_component::<Vitality>();
    app.register_mod_component::<Resistance>();
    app.register_mod_component::<AuraEmitter>();
    app.register_mod_component::<Faction>();
    app.register_mod_component::<EmitterBeaconTag>();
    app.register_mod_component::<CombatUnitTag>();
    app.register_mod_component::<FrozenFlag>();
    app.register_mod_component::<ArmorPlating>();
    app.register_mod_component::<JetpackModule>();
    app.register_mod_component::<PsionicWard>();
    app.register_mod_component::<SquadBanner>();
    app.register_mod_component::<StealthCloak>();
    app.register_mod_component::<TelemetryTag>();
}

/// Registers all 15 components directly on a pure-Bevy `World` (no `AbiModPlugin` / `ModRegistry`).
pub fn register_pure_native_components(world: &mut World) {
    world.register_component::<Transform3D>();
    world.register_component::<Kinematics>();
    world.register_component::<Vitality>();
    world.register_component::<Resistance>();
    world.register_component::<AuraEmitter>();
    world.register_component::<Faction>();
    world.register_component::<EmitterBeaconTag>();
    world.register_component::<CombatUnitTag>();
    world.register_component::<FrozenFlag>();
    world.register_component::<ArmorPlating>();
    world.register_component::<JetpackModule>();
    world.register_component::<PsionicWard>();
    world.register_component::<SquadBanner>();
    world.register_component::<StealthCloak>();
    world.register_component::<TelemetryTag>();
}

// ============================================================================
// 2. Deterministic 34-Archetype World Population
// ============================================================================

#[derive(Clone, Copy, Debug)]
pub struct ScenarioConfig {
    /// Entities per each of the 16 active CombatUnit archetypes.
    pub units_per_archetype: usize,
    /// Entities per each of the 6 active AuraEmitter archetypes.
    pub emitters_per_archetype: usize,
    /// Entities per each of the 12 excluded/frozen/decoy archetypes.
    pub decoys_per_archetype: usize,
}

impl ScenarioConfig {
    /// Oracle correctness check scale: 256 active units × 24 active emitters = 6,144 pairs/tick.
    pub const fn oracle() -> Self {
        Self {
            units_per_archetype: 16,
            emitters_per_archetype: 4,
            decoys_per_archetype: 8,
        }
    }

    /// Standard cross-target matrix scale: 1,024 active units × 48 active emitters = 49,152 pairs/tick
    /// across 34 distinct Bevy Archetype Tables (fast enough for Debug+Debug, rich enough for sub-percent precision).
    pub const fn standard() -> Self {
        Self {
            units_per_archetype: 64,
            emitters_per_archetype: 8,
            decoys_per_archetype: 16,
        }
    }

    /// High-scale Release stress scale: 3,072 active units × 96 active emitters = 294,912 pairs/tick.
    pub const fn heavy() -> Self {
        Self {
            units_per_archetype: 192,
            emitters_per_archetype: 16,
            decoys_per_archetype: 32,
        }
    }

    pub const fn active_units(&self) -> usize {
        self.units_per_archetype * 16
    }

    pub const fn active_emitters(&self) -> usize {
        self.emitters_per_archetype * 6
    }

    pub const fn pair_evaluations_per_frame(&self) -> usize {
        self.active_units() * self.active_emitters()
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ScenarioWorldStats {
    pub active_units: usize,
    pub active_emitters: usize,
    pub excluded_entities: usize,
    pub total_entities: usize,
    pub total_archetypes: usize,
    pub active_unit_archetypes: usize,
    pub active_emitter_archetypes: usize,
    pub pair_evaluations_per_frame: usize,
}

fn make_unit_base(
    serial: u32,
) -> (
    Transform3D,
    Kinematics,
    Vitality,
    Resistance,
    Faction,
    CombatUnitTag,
) {
    let s = serial as f32;
    // Deterministic Golden-angle spiral distribution inside [-45, 45]^3 arena
    let px = ((s * 0.618_034).fract() * 90.0) - 45.0;
    let py = ((s * 0.381_966).fract() * 40.0) - 20.0;
    let pz = ((s * 0.754_877).fract() * 90.0) - 45.0;

    (
        Transform3D {
            position: Vec3f {
                x: px,
                y: py,
                z: pz,
            },
            forward: Vec3f {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            scale: 1.0 + (serial % 4) as f32 * 0.25,
        },
        Kinematics {
            velocity: Vec3f {
                x: ((serial % 7) as f32 - 3.0) * 0.5,
                y: 0.0,
                z: ((serial % 5) as f32 - 2.0) * 0.5,
            },
            drag: 0.15 + (serial % 3) as f32 * 0.05,
            max_speed: 35.0,
        },
        Vitality {
            health: 85.0,
            max_health: 120.0,
            shield: 30.0,
            energy: 50.0,
        },
        Resistance {
            kinetic: 0.10 + (serial % 5) as f32 * 0.08,
            thermal: 0.15 + (serial % 4) as f32 * 0.10,
            ionic: 0.05 + (serial % 6) as f32 * 0.07,
        },
        Faction {
            team_id: serial % 3,
            serial_id: serial,
        },
        CombatUnitTag {
            unit_class: serial % 8,
        },
    )
}

fn make_emitter_base(serial: u32) -> (Transform3D, AuraEmitter, Faction, EmitterBeaconTag) {
    let s = serial as f32;
    let ex = ((s * 0.414_213).fract() * 80.0) - 40.0;
    let ey = ((s * 0.732_050).fract() * 30.0) - 15.0;
    let ez = ((s * 0.236_067).fract() * 80.0) - 40.0;

    // Normalized forward vector
    let fx_raw = (serial % 5) as f32 - 2.0;
    let fy_raw = 0.5_f32;
    let fz_raw = (serial % 3) as f32 - 1.0;
    let fn_len = (fx_raw * fx_raw + fy_raw * fy_raw + fz_raw * fz_raw).sqrt();

    (
        Transform3D {
            position: Vec3f {
                x: ex,
                y: ey,
                z: ez,
            },
            forward: Vec3f {
                x: fx_raw / fn_len,
                y: fy_raw / fn_len,
                z: fz_raw / fn_len,
            },
            scale: 2.0,
        },
        AuraEmitter {
            radius: 48.0 + (serial % 4) as f32 * 8.0,
            force: 18.0 + (serial % 3) as f32 * 4.0,
            thermal_dps: 22.0 + (serial % 5) as f32 * 3.0,
            heal_hps: 14.0 + (serial % 4) as f32 * 2.5,
            falloff: 0.004,
        },
        Faction {
            team_id: serial % 3,
            serial_id: 100_000 + serial,
        },
        EmitterBeaconTag {
            channel: serial % 4,
        },
    )
}

/// Populates `world` with 34 distinct Bevy Archetype Tables:
/// - 16 Active Combat Unit archetypes (all 2^4 subsets of Armor/Jetpack/Psionic/SquadBanner)
/// - 6 Active Emitter Beacon archetypes
/// - 2 Frozen Emitter archetypes (excluded via `Without<FrozenFlag>`)
/// - 4 Frozen / Incomplete Unit archetypes (excluded via `Without<FrozenFlag>` or missing `Resistance`)
/// - 6 Decoy Scenery / Prop archetypes (excluded via missing `CombatUnitTag` / `EmitterBeaconTag`)
pub fn populate_scenario_world(world: &mut World, cfg: ScenarioConfig) -> ScenarioWorldStats {
    let mut unit_serial: u32 = 0;

    // 1. 16 Active Unit Archetypes (bitmask 0..16 over 4 optional payload components)
    for mask in 0..16_u32 {
        for _ in 0..cfg.units_per_archetype {
            let base = make_unit_base(unit_serial);
            unit_serial += 1;
            let mut ent = world.spawn(base);
            if (mask & 1) != 0 {
                ent.insert(ArmorPlating { thickness: 12.5 });
            }
            if (mask & 2) != 0 {
                ent.insert(JetpackModule { fuel: 75.0 });
            }
            if (mask & 4) != 0 {
                ent.insert(PsionicWard { resonance: 0.8 });
            }
            if (mask & 8) != 0 {
                ent.insert(SquadBanner { squad_id: mask });
            }
        }
    }

    // 2. 6 Active Emitter Archetypes
    let mut emitter_serial: u32 = 0;
    for arch in 0..6_u32 {
        for _ in 0..cfg.emitters_per_archetype {
            let base = make_emitter_base(emitter_serial);
            emitter_serial += 1;
            let mut ent = world.spawn(base);
            match arch {
                0 => {}
                1 => {
                    ent.insert(ArmorPlating { thickness: 40.0 });
                }
                2 => {
                    ent.insert(PsionicWard { resonance: 1.5 });
                }
                3 => {
                    ent.insert(SquadBanner { squad_id: 99 });
                }
                4 => {
                    ent.insert((
                        ArmorPlating { thickness: 25.0 },
                        PsionicWard { resonance: 2.0 },
                    ));
                }
                5 => {
                    ent.insert(TelemetryTag {
                        label: format!("beacon_{emitter_serial}"),
                    });
                }
                _ => unreachable!(),
            }
        }
    }

    // 3. 12 Excluded / Frozen / Decoy Archetypes (must never be mutated or read as active!)
    let mut excluded_count = 0_usize;
    let mut decoy_serial = 500_000_u32;

    // 3a. 2 Frozen Emitter archetypes (`FrozenFlag` present -> excluded by `Without<FrozenFlag>`)
    for arch in 0..2_u32 {
        for _ in 0..cfg.decoys_per_archetype {
            let base = make_emitter_base(decoy_serial);
            decoy_serial += 1;
            excluded_count += 1;
            let mut ent = world.spawn((base, FrozenFlag { remaining_ticks: 5 }));
            if arch == 1 {
                ent.insert(ArmorPlating { thickness: 50.0 });
            }
        }
    }

    // 3b. 2 Frozen Unit archetypes (`FrozenFlag` present -> excluded by `Without<FrozenFlag>`)
    for arch in 0..2_u32 {
        for _ in 0..cfg.decoys_per_archetype {
            let base = make_unit_base(decoy_serial);
            decoy_serial += 1;
            excluded_count += 1;
            let mut ent = world.spawn((base, FrozenFlag { remaining_ticks: 8 }));
            if arch == 1 {
                ent.insert(JetpackModule { fuel: 10.0 });
            }
        }
    }

    // 3c. 2 Incomplete Unit archetypes (missing `Resistance` -> excluded by Outer Query)
    for arch in 0..2_u32 {
        for _ in 0..cfg.decoys_per_archetype {
            let (tf, kin, vit, _res, fac, tag) = make_unit_base(decoy_serial);
            decoy_serial += 1;
            excluded_count += 1;
            let mut ent = world.spawn((tf, kin, vit, fac, tag));
            if arch == 1 {
                ent.insert(StealthCloak { opacity: 0.9 });
            }
        }
    }

    // 3d. 6 Decoy Scenery / Particle / Trigger archetypes (missing `CombatUnitTag` & `EmitterBeaconTag`)
    for arch in 0..6_u32 {
        for _ in 0..cfg.decoys_per_archetype {
            let (tf, kin, vit, _res, fac, _tag) = make_unit_base(decoy_serial);
            decoy_serial += 1;
            excluded_count += 1;
            match arch {
                0 => {
                    world.spawn(tf);
                }
                1 => {
                    world.spawn((tf, kin));
                }
                2 => {
                    world.spawn((tf, StealthCloak { opacity: 0.5 }));
                }
                3 => {
                    world.spawn((
                        tf,
                        TelemetryTag {
                            label: format!("prop_{decoy_serial}"),
                        },
                    ));
                }
                4 => {
                    world.spawn((tf, vit, fac));
                }
                5 => {
                    world.spawn((
                        tf,
                        kin,
                        ArmorPlating { thickness: 5.0 },
                        StealthCloak { opacity: 0.2 },
                    ));
                }
                _ => unreachable!(),
            }
        }
    }

    let active_units = unit_serial as usize;
    let active_emitters = emitter_serial as usize;
    let total_entities = active_units + active_emitters + excluded_count;
    // +1 or +2 for Bevy's empty root archetype; total >= 34
    let total_archetypes = world.archetypes().len();

    ScenarioWorldStats {
        active_units,
        active_emitters,
        excluded_entities: excluded_count,
        total_entities,
        total_archetypes,
        active_unit_archetypes: 16,
        active_emitter_archetypes: 6,
        pair_evaluations_per_frame: active_units * active_emitters,
    }
}

// ============================================================================
// 3. Pure Native Bevy Nested Multi-Archetype System
// ============================================================================

/// Pure Native Bevy ECS nested multi-archetype system (Direct Idiomatic Entity-Nested `Query`).
///
/// Outer query traverses 16 active unit archetypes; inner query traverses 6 active emitter archetypes
/// per unit, while skipping 12 frozen/incomplete/scenery archetypes.
pub fn native_nested_aura_combat_system(
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

        // Integrate drag, speed clamp, position, and orientation
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

/// Pure Native Bevy ECS nested multi-archetype system with SoA Emitter Pre-Cache (`Local<Vec<...>>`).
///
/// Represents the absolute hardware ceiling of Pure Native Bevy when a Rust gameplay programmer
/// flattens the 6 fragmented emitter archetypes into an L1-resident contiguous `Local<Vec<CachedEmitter>>`
/// once per frame before running the 16-archetype outer `Query`.
#[derive(Clone, Copy)]
pub struct CachedNativeEmitter {
    pub px: f32,
    pub py: f32,
    pub pz: f32,
    pub fx: f32,
    pub fy: f32,
    pub fz: f32,
    pub radius: f32,
    pub rad_sq: f32,
    pub force: f32,
    pub thermal_dps: f32,
    pub heal_hps: f32,
    pub falloff: f32,
    pub team_id: u32,
}

pub fn native_cached_soa_nested_aura_combat_system(
    mut cached_emitters: bevy_ecs::system::Local<Vec<CachedNativeEmitter>>,
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

// ============================================================================
// 4. Mathematical Correctness Oracle (Verifies Every Permutation Matches Native)
// ============================================================================

#[derive(Clone, Debug)]
pub struct UnitSnapshot {
    pub serial_id: u32,
    pub pos: [f32; 3],
    pub fwd: [f32; 3],
    pub vel: [f32; 3],
    pub health: f32,
    pub shield: f32,
    pub energy: f32,
}

#[derive(Clone, Debug)]
pub struct OracleDigest {
    pub units: Vec<UnitSnapshot>,
    pub frozen_checksum: f64,
}

impl OracleDigest {
    pub fn capture(world: &mut World) -> Self {
        let mut unit_query = world
            .query_filtered::<(&Transform3D, &Kinematics, &Vitality, &Faction), (
                With<CombatUnitTag>,
                Without<EmitterBeaconTag>,
                Without<FrozenFlag>,
            )>();

        let mut units: Vec<UnitSnapshot> = unit_query
            .iter(world)
            .map(|(tf, kin, vit, fac)| UnitSnapshot {
                serial_id: fac.serial_id,
                pos: [tf.position.x, tf.position.y, tf.position.z],
                fwd: [tf.forward.x, tf.forward.y, tf.forward.z],
                vel: [kin.velocity.x, kin.velocity.y, kin.velocity.z],
                health: vit.health,
                shield: vit.shield,
                energy: vit.energy,
            })
            .collect();

        units.sort_by_key(|u| u.serial_id);

        // Also capture checksum of excluded Frozen entities to prove filter soundness
        let mut frozen_query = world.query_filtered::<&Transform3D, With<FrozenFlag>>();
        let mut frozen_checksum = 0.0_f64;
        for tf in frozen_query.iter(world) {
            frozen_checksum += (tf.position.x as f64) * 1.1
                + (tf.position.y as f64) * 2.3
                + (tf.position.z as f64) * 3.7;
        }

        Self {
            units,
            frozen_checksum,
        }
    }

    pub fn assert_equivalent(&self, candidate: &Self, label: &str) {
        assert_eq!(
            self.units.len(),
            candidate.units.len(),
            "[{label}] active unit count mismatch vs Pure Native oracle"
        );
        assert!(
            (self.frozen_checksum - candidate.frozen_checksum).abs() < 1e-6,
            "[{label}] FrozenFlag entities were erroneously mutated! expected {}, got {}",
            self.frozen_checksum,
            candidate.frozen_checksum
        );

        let eps = 1.5e-3_f32;
        for (a, b) in self.units.iter().zip(candidate.units.iter()) {
            assert_eq!(a.serial_id, b.serial_id, "[{label}] serial_id mismatch");
            for axis in 0..3 {
                assert!(
                    (a.pos[axis] - b.pos[axis]).abs() <= eps,
                    "[{label}] unit {} pos[{axis}] mismatch: native={} vs candidate={}",
                    a.serial_id,
                    a.pos[axis],
                    b.pos[axis]
                );
                assert!(
                    (a.vel[axis] - b.vel[axis]).abs() <= eps,
                    "[{label}] unit {} vel[{axis}] mismatch: native={} vs candidate={}",
                    a.serial_id,
                    a.vel[axis],
                    b.vel[axis]
                );
                assert!(
                    (a.fwd[axis] - b.fwd[axis]).abs() <= eps,
                    "[{label}] unit {} fwd[{axis}] mismatch: native={} vs candidate={}",
                    a.serial_id,
                    a.fwd[axis],
                    b.fwd[axis]
                );
            }
            assert!(
                (a.health - b.health).abs() <= eps,
                "[{label}] unit {} health mismatch: native={} vs candidate={}",
                a.serial_id,
                a.health,
                b.health
            );
            assert!(
                (a.shield - b.shield).abs() <= eps,
                "[{label}] unit {} shield mismatch: native={} vs candidate={}",
                a.serial_id,
                a.shield,
                b.shield
            );
            assert!(
                (a.energy - b.energy).abs() <= eps,
                "[{label}] unit {} energy mismatch: native={} vs candidate={}",
                a.serial_id,
                a.energy,
                b.energy
            );
        }
    }
}
