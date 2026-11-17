#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]
#![allow(clippy::type_complexity, unreachable_pub)]

//! Shared Multi-Archetype Nested Query Benchmark Scenario (`ABI 0.2.0`).
//!
//! Compiled as a shared Rust `dylib` so that:
//! 1. `permutation_bench` (host binary) runs Pure Native Bevy baselines and C-ABI mods.
//! 2. `coupled_rust_mod` (coupled Rust `dylib` plugin) dynamically links against the same
//!    `bevy_ecs`, `bevy_app`, and component `TypeId` definitions in memory — modeling unstable
//!    Rust-ABI dynamic plugins (`bevy_dynamic_plugin` style).

pub use bevy_abi_mod_host;
pub use bevy_app;
pub use bevy_ecs;

use bevy_abi_mod_host::{ComponentOptions, ModAppExt, ModComponent};
use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::query::{With, Without};
use bevy_ecs::system::Query;
use bevy_ecs::world::World;

/// Fixed simulation timestep in seconds.
pub const SIM_DT: f32 = 0.016_f32;

// ============================================================================
// 1. Nested Compound Structs & ECS Components (`#[derive(ModComponent)]`)
// ============================================================================

/// Nested 3D vector exposed via interior pointers (`ptr_<field>`) and `Vec3fVTable`.
#[derive(Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Vec3f {
    /// X component.
    pub x: f32,
    /// Y component.
    pub y: f32,
    /// Z component.
    pub z: f32,
}

/// 3D spatial transform containing two nested `Vec3f` structs (`position` and `forward`).
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Transform3D {
    /// World-space position.
    pub position: Vec3f,
    /// Normalized forward direction.
    pub forward: Vec3f,
    /// Uniform scale factor.
    pub scale: f32,
}

/// Linear kinematics containing a nested `Vec3f` velocity, drag coefficient, and speed cap.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Kinematics {
    /// Linear velocity vector.
    pub velocity: Vec3f,
    /// Linear drag coefficient.
    pub drag: f32,
    /// Maximum speed cap.
    pub max_speed: f32,
}

/// Multi-pool combat vitality state mutated by friendly/hostile aura interactions.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Vitality {
    /// Current health pool.
    pub health: f32,
    /// Maximum health cap.
    pub max_health: f32,
    /// Regenerative shield pool.
    pub shield: f32,
    /// Auxiliary energy pool.
    pub energy: f32,
}

/// Damage and impulse mitigation resistances.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Resistance {
    /// Kinetic impulse mitigation factor.
    pub kinetic: f32,
    /// Thermal damage mitigation factor.
    pub thermal: f32,
    /// Ionic drain/heal modifier.
    pub ionic: f32,
}

/// Spatial field emitter parameters read by the inner nested query.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct AuraEmitter {
    /// Effective field radius.
    pub radius: f32,
    /// Radial/directional force strength.
    pub force: f32,
    /// Hostile thermal damage per second.
    pub thermal_dps: f32,
    /// Friendly heal rate per second.
    pub heal_hps: f32,
    /// Quadratic distance falloff coefficient.
    pub falloff: f32,
}

/// Faction alignment (`team_id`) and deterministic entity serial ID (`serial_id`).
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct Faction {
    /// Team identifier.
    pub team_id: u32,
    /// Deterministic entity serial number for oracle sorting.
    pub serial_id: u32,
}

// --- Archetype Filter & Fragmentation Components ---

/// Marker payload identifying active aura emitter entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct EmitterBeaconTag {
    /// Broadcast channel index.
    pub channel: u32,
}

/// Marker payload identifying active combat unit entities.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct CombatUnitTag {
    /// Unit class identifier.
    pub unit_class: u32,
}

/// Stasis marker excluding frozen entities via `Without<FrozenFlag>`.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct FrozenFlag {
    /// Remaining stasis ticks.
    pub remaining_ticks: u32,
}

/// Optional archetype fragmentation payload: armor plating.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct ArmorPlating {
    /// Plating thickness.
    pub thickness: f32,
}

/// Optional archetype fragmentation payload: jetpack module.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct JetpackModule {
    /// Remaining fuel.
    pub fuel: f32,
}

/// Optional archetype fragmentation payload: psionic ward.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct PsionicWard {
    /// Ward resonance strength.
    pub resonance: f32,
}

/// Optional archetype fragmentation payload: squad banner.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct SquadBanner {
    /// Assigned squad identifier.
    pub squad_id: u32,
}

/// Optional archetype fragmentation payload: stealth cloak.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct StealthCloak {
    /// Cloak opacity.
    pub opacity: f32,
}

/// Heap-owning string label component exercising non-POD VTable registration.
#[derive(Component, Debug, Clone, PartialEq, ModComponent)]
#[mod_component(namespace = "shared_scenario")]
pub struct TelemetryTag {
    /// Human-readable telemetry tag string.
    pub label: String,
}

/// Registers all 15 components and the nested `Vec3f` compound type into `App` and `ModRegistry`.
pub fn register_all_scenario_components(app: &mut App) {
    app.register_mod_type::<Vec3f>();
    app.register_mod_component::<Transform3D>(ComponentOptions::new())
        .expect("register Transform3D");
    app.register_mod_component::<Kinematics>(ComponentOptions::new())
        .expect("register Kinematics");
    app.register_mod_component::<Vitality>(ComponentOptions::new())
        .expect("register Vitality");
    app.register_mod_component::<Resistance>(ComponentOptions::new())
        .expect("register Resistance");
    app.register_mod_component::<AuraEmitter>(ComponentOptions::new())
        .expect("register AuraEmitter");
    app.register_mod_component::<Faction>(ComponentOptions::new())
        .expect("register Faction");
    app.register_mod_component::<EmitterBeaconTag>(ComponentOptions::new())
        .expect("register EmitterBeaconTag");
    app.register_mod_component::<CombatUnitTag>(ComponentOptions::new())
        .expect("register CombatUnitTag");
    app.register_mod_component::<FrozenFlag>(ComponentOptions::new())
        .expect("register FrozenFlag");
    app.register_mod_component::<ArmorPlating>(ComponentOptions::new())
        .expect("register ArmorPlating");
    app.register_mod_component::<JetpackModule>(ComponentOptions::new())
        .expect("register JetpackModule");
    app.register_mod_component::<PsionicWard>(ComponentOptions::new())
        .expect("register PsionicWard");
    app.register_mod_component::<SquadBanner>(ComponentOptions::new())
        .expect("register SquadBanner");
    app.register_mod_component::<StealthCloak>(ComponentOptions::new())
        .expect("register StealthCloak");
    app.register_mod_component::<TelemetryTag>(ComponentOptions::new())
        .expect("register TelemetryTag");
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

/// Population scale parameters for the 34-archetype benchmark world.
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
    /// across 34 distinct Bevy Archetype Tables.
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

    /// Total active combat units across all 16 unit archetypes.
    pub const fn active_units(&self) -> usize {
        self.units_per_archetype * 16
    }

    /// Total active aura emitters across all 6 emitter archetypes.
    pub const fn active_emitters(&self) -> usize {
        self.emitters_per_archetype * 6
    }

    /// Total unit-emitter pair evaluations per tick.
    pub const fn pair_evaluations_per_frame(&self) -> usize {
        self.active_units() * self.active_emitters()
    }
}

/// Summary statistics returned after populating a benchmark `World`.
#[derive(Clone, Copy, Debug)]
pub struct ScenarioWorldStats {
    /// Number of active combat units.
    pub active_units: usize,
    /// Number of active aura emitters.
    pub active_emitters: usize,
    /// Number of decoy or frozen entities excluded by filters.
    pub excluded_entities: usize,
    /// Total spawned entities.
    pub total_entities: usize,
    /// Total archetype count in the Bevy `World`.
    pub total_archetypes: usize,
    /// Number of active unit archetypes (`16`).
    pub active_unit_archetypes: usize,
    /// Number of active emitter archetypes (`6`).
    pub active_emitter_archetypes: usize,
    /// Total pair evaluations per frame (`active_units * active_emitters`).
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

    // 1. 16 Active Unit Archetypes
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

    // 3. 12 Excluded / Frozen / Decoy Archetypes
    let mut excluded_count = 0_usize;
    let mut decoy_serial = 500_000_u32;

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

/// Pure Native Bevy ECS nested multi-archetype system (direct entity-nested `Query`).
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

/// Flattened emitter record used by [`native_cached_soa_nested_aura_combat_system`].
#[derive(Clone, Copy)]
pub struct CachedNativeEmitter {
    /// Emitter X coordinate.
    pub px: f32,
    /// Emitter Y coordinate.
    pub py: f32,
    /// Emitter Z coordinate.
    pub pz: f32,
    /// Forward X component.
    pub fx: f32,
    /// Forward Y component.
    pub fy: f32,
    /// Forward Z component.
    pub fz: f32,
    /// Effective field radius.
    pub radius: f32,
    /// Precomputed `radius * radius`.
    pub rad_sq: f32,
    /// Force magnitude.
    pub force: f32,
    /// Hostile thermal damage per second.
    pub thermal_dps: f32,
    /// Friendly heal rate per second.
    pub heal_hps: f32,
    /// Quadratic distance falloff.
    pub falloff: f32,
    /// Emitter faction identifier.
    pub team_id: u32,
}

/// Pure Native Bevy ECS nested multi-archetype system with `Local<Vec<CachedNativeEmitter>>` pre-cache.
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
// 4. Mathematical Correctness Oracle
// ============================================================================

/// Per-unit state snapshot captured for mathematical equivalence verification.
#[derive(Clone, Debug)]
pub struct UnitSnapshot {
    /// Deterministic unit serial identifier.
    pub serial_id: u32,
    /// World-space `[x, y, z]` position.
    pub pos: [f32; 3],
    /// Normalized `[x, y, z]` forward vector.
    pub fwd: [f32; 3],
    /// Linear `[x, y, z]` velocity vector.
    pub vel: [f32; 3],
    /// Health value.
    pub health: f32,
    /// Shield value.
    pub shield: f32,
    /// Energy value.
    pub energy: f32,
}

/// Complete sorted digest of active units and excluded frozen entities.
#[derive(Clone, Debug)]
pub struct OracleDigest {
    /// Sorted active unit snapshots.
    pub units: Vec<UnitSnapshot>,
    /// Checksum over `FrozenFlag` entities verifying filter exclusion.
    pub frozen_checksum: f64,
}

impl OracleDigest {
    /// Captures an [`OracleDigest`] from `world`.
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

    /// Asserts numerical equivalence between `self` (native baseline) and `candidate`.
    pub fn assert_equivalent(&self, candidate: &Self, label: &str) {
        assert_eq!(
            self.units.len(),
            candidate.units.len(),
            "[{label}] active unit count mismatch vs Pure Native oracle"
        );
        assert!(
            (self.frozen_checksum - candidate.frozen_checksum).abs() < 1e-6,
            "[{label}] FrozenFlag entities were erroneously mutated: expected {}, got {}",
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
