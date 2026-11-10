//! Empirical Adversarial Verification Suite for Milestone 3 (Examples 1, 2, and 3)
//!
//! Stress-tests:
//! 1. Example 1: Repeated lifecycle reloading, rapid load-tick-unload loops, empty and large entity chunks.
//! 2. Example 2: Deferred command execution, attrition despawn, reinforcement spawning, WITHOUT/FILTER_ONLY isolation.
//! 3. Example 3: ISO C99 + Rust mod concurrency, memory stomping checks, asymmetric unloads, post-unload stability.

#![allow(clippy::default_constructed_unit_structs)]

use bevy_app::App;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use host_runtime::prelude::*;
use mod_derive::ModExposable;
use std::path::PathBuf;
use std::process::Command;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct PlayerTag {
    pub id: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct DroneTag {
    pub squad: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct ColorRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn get_example_1_dylib() -> PathBuf {
    let root = get_workspace_root();
    let manifest = root.join("examples/01_minimal_handshake/guest_mod/Cargo.toml");
    let dylib = root
        .join("examples/01_minimal_handshake/guest_mod/target/release/libminimal_guest_mod.dylib");
    if !dylib.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--release")
            .status()
            .expect("Failed to build Example 1 guest mod");
        assert!(status.success());
    }
    dylib
}

fn get_example_2_dylib() -> PathBuf {
    let root = get_workspace_root();
    let manifest = root.join("examples/02_multi_system_pipeline/guest_mod/Cargo.toml");
    let dylib = root.join(
        "examples/02_multi_system_pipeline/guest_mod/target/release/libexample_02_guest_mod.dylib",
    );
    if !dylib.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--release")
            .status()
            .expect("Failed to build Example 2 guest mod");
        assert!(status.success());
    }
    dylib
}

fn get_example_3_dylibs() -> (PathBuf, PathBuf) {
    let root = get_workspace_root();
    let c_mod_dir = root.join("examples/03_cross_language_interop/c_mod");
    let c_dylib = c_mod_dir.join("libc_mod.dylib");
    if !c_dylib.exists() {
        let status = Command::new("make")
            .arg("-C")
            .arg(&c_mod_dir)
            .status()
            .expect("Failed to build Example 3 C mod");
        assert!(status.success());
    }

    let rust_manifest = root.join("examples/03_cross_language_interop/rust_mod/Cargo.toml");
    let rust_dylib =
        root.join("examples/03_cross_language_interop/rust_mod/target/release/librust_mod.dylib");
    if !rust_dylib.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&rust_manifest)
            .arg("--release")
            .status()
            .expect("Failed to build Example 3 Rust mod");
        assert!(status.success());
    }

    (c_dylib, rust_dylib)
}

// ==============================================================================
// CHALLENGE 1: EXAMPLE 1 STRESS VERIFICATION
// ==============================================================================

#[test]
fn test_adversarial_example_1_repeated_lifecycle_reload() {
    let dylib = get_example_1_dylib();
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();

    // Spawn test entities
    let mut entities = Vec::new();
    for i in 0..10 {
        let e = app
            .world_mut()
            .spawn(Position {
                x: (i * 10) as f32,
                y: (i * 20) as f32,
                z: (i * 30) as f32,
            })
            .id();
        entities.push(e);
    }

    // Flush initial spawn
    app.update();

    // Perform 15 consecutive load -> tick 3x -> unload cycles
    for cycle in 1..=15 {
        let mod_id = app
            .load_mod_tracked(&dylib)
            .unwrap_or_else(|e| panic!("Failed to load mod in cycle {}: {:?}", cycle, e));

        // 3 active ticks
        for _ in 0..3 {
            app.update();
        }

        // Unload mod
        app.unload_mod(mod_id)
            .unwrap_or_else(|e| panic!("Failed to unload mod in cycle {}: {:?}", cycle, e));

        // Record positions after unload
        let pos_before_post_tick: Vec<Position> = entities
            .iter()
            .map(|&e| *app.world().get::<Position>(e).unwrap())
            .collect();

        // 1 post-unload tick: must produce ZERO changes
        app.update();

        let pos_after_post_tick: Vec<Position> = entities
            .iter()
            .map(|&e| *app.world().get::<Position>(e).unwrap())
            .collect();

        assert_eq!(
            pos_before_post_tick, pos_after_post_tick,
            "Cycle {}: Post-unload tick must not modify Position",
            cycle
        );
    }

    // Total active ticks across 15 cycles: 15 * 3 = 45 ticks.
    // Each tick adds (+1.0, +2.0, +3.0).
    // Final position for entity 0: (45.0, 90.0, 135.0).
    let e0_pos = *app.world().get::<Position>(entities[0]).unwrap();
    assert!(
        (e0_pos.x - 45.0).abs() < 1e-3,
        "Entity 0 X should be 45.0 after 45 active ticks, got {}",
        e0_pos.x
    );
    assert!(
        (e0_pos.y - 90.0).abs() < 1e-3,
        "Entity 0 Y should be 90.0 after 45 active ticks, got {}",
        e0_pos.y
    );
    assert!(
        (e0_pos.z - 135.0).abs() < 1e-3,
        "Entity 0 Z should be 135.0 after 45 active ticks, got {}",
        e0_pos.z
    );
}

#[test]
fn test_adversarial_example_1_empty_and_large_chunk_stress() {
    let dylib = get_example_1_dylib();
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();

    let mod_id = app.load_mod_tracked(&dylib).expect("load mod");

    // Stress 1: Run 5 ticks with 0 entities in the world
    for _ in 0..5 {
        app.update();
    }

    // Stress 2: Spawn 2,000 entities in chunks
    let mut test_entities = Vec::with_capacity(2000);
    for i in 0..2000 {
        let e = app
            .world_mut()
            .spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            })
            .id();
        test_entities.push(e);
    }

    // Run 5 ticks
    for _ in 0..5 {
        app.update();
    }

    // Check all 2,000 entities
    for (i, &e) in test_entities.iter().enumerate() {
        let pos = *app.world().get::<Position>(e).unwrap();
        let expected_x = i as f32 + 5.0 * 1.0;
        let expected_y = 5.0 * 2.0;
        let expected_z = 5.0 * 3.0;
        assert!(
            (pos.x - expected_x).abs() < 1e-3,
            "Entity {} X mismatch: expected {}, got {}",
            i,
            expected_x,
            pos.x
        );
        assert!(
            (pos.y - expected_y).abs() < 1e-3,
            "Entity {} Y mismatch: expected {}, got {}",
            i,
            expected_y,
            pos.y
        );
        assert!(
            (pos.z - expected_z).abs() < 1e-3,
            "Entity {} Z mismatch: expected {}, got {}",
            i,
            expected_z,
            pos.z
        );
    }

    app.unload_mod(mod_id).expect("unload mod");
}

// ==============================================================================
// CHALLENGE 2: EXAMPLE 2 MULTI-SYSTEM PIPELINE & DEFERRED COMMANDS
// ==============================================================================

#[test]
fn test_adversarial_example_2_deferred_commands_and_attrition_stress() {
    let dylib = get_example_2_dylib();
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Health>();
    app.register_mod_component::<PlayerTag>();
    app.register_mod_component::<DroneTag>();

    // 1. Player entity (80 HP, max 100 HP)
    let player = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 0.0,
            },
            Health {
                current: 80.0,
                max: 100.0,
            },
            PlayerTag { id: 1 },
        ))
        .id();

    // 2. 20 low-health drones (10 HP -> will be despawned on tick 1)
    let mut low_hp_drones = Vec::new();
    for i in 0..20 {
        let d = app
            .world_mut()
            .spawn((
                Position {
                    x: (10 + i) as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 0.0,
                    y: 1.0,
                    z: 0.0,
                },
                Health {
                    current: 10.0,
                    max: 50.0,
                },
                DroneTag { squad: 1 },
            ))
            .id();
        low_hp_drones.push(d);
    }

    // 3. 20 high-health drones (100 HP -> survive all 5 ticks)
    let mut high_hp_drones = Vec::new();
    for i in 0..20 {
        let d = app
            .world_mut()
            .spawn((
                Position {
                    x: (100 + i) as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 2.0,
                    y: 0.0,
                    z: 0.0,
                },
                Health {
                    current: 100.0,
                    max: 100.0,
                },
                DroneTag { squad: 2 },
            ))
            .id();
        high_hp_drones.push(d);
    }

    // Flush initial spawn
    app.update();

    let mod_id = app.load_mod_tracked(&dylib).expect("load example 2 mod");

    // Tick 1:
    // - Low HP drones take -10 HP -> reach 0 HP -> queue.despawn(e)
    // - High HP drones take -10 HP -> reach 90 HP
    // - Player heals +5 HP -> reach 85 HP
    // - Reinforcement spawner queues spawn_empty + insert_values for Drone Gamma
    app.update();

    // Verify: ALL 20 low-health drones must now be completely despawned!
    for (i, &d) in low_hp_drones.iter().enumerate() {
        assert!(
            app.world().get_entity(d).is_err(),
            "Low-HP drone {} ({:?}) must be despawned after tick 1",
            i,
            d
        );
    }

    // Verify: High-health drones took damage and survived
    for &d in &high_hp_drones {
        let h = app
            .world()
            .get::<Health>(d)
            .expect("high hp drone still exists");
        assert!(
            (h.current - 90.0).abs() < 1e-3,
            "High-HP drone should have 90.0 HP on tick 1, got {}",
            h.current
        );
    }

    // Verify: Player was healed
    let p_hp = app.world().get::<Health>(player).unwrap();
    assert!(
        (p_hp.current - 85.0).abs() < 1e-3,
        "Player should have 85.0 HP on tick 1, got {}",
        p_hp.current
    );

    // Verify: Reinforcement drone was spawned deferred
    let mut reinforcement_count = 0;
    let mut query = app
        .world_mut()
        .query::<(Entity, &Position, &Velocity, &Health, &DroneTag)>();
    for (e, pos, vel, health, tag) in query.iter(app.world()) {
        if tag.squad == 2 && !high_hp_drones.contains(&e) {
            reinforcement_count += 1;
            assert!((pos.x - 30.0).abs() < 1e-3 || (pos.x - 30.2).abs() < 1e-3);
            assert!((vel.x - 2.0).abs() < 1e-3);
            assert!((health.max - 50.0).abs() < 1e-3);
        }
    }
    assert_eq!(
        reinforcement_count, 1,
        "Exactly 1 reinforcement drone must be spawned"
    );

    // Run remaining 4 ticks (total 5 ticks)
    for _ in 2..=5 {
        app.update();
    }

    // After 5 ticks:
    // - Player heals: 80 -> 85 -> 90 -> 95 -> 100 -> 100 (capped at max 100)
    let p_hp_final = app.world().get::<Health>(player).unwrap();
    assert_eq!(
        p_hp_final.current, 100.0,
        "Player health capped at max 100.0"
    );

    // - High HP drones took 5 * 10 = 50 damage -> 50 HP remaining
    for &d in &high_hp_drones {
        let h = app.world().get::<Health>(d).unwrap();
        assert!(
            (h.current - 50.0).abs() < 1e-3,
            "High-HP drone should have 50.0 HP on tick 5, got {}",
            h.current
        );
    }

    // Clean unload
    app.unload_mod(mod_id).expect("unload example 2 mod");

    // Post-unload tick: zero drift
    let p_hp_post = *app.world().get::<Health>(player).unwrap();
    app.update();
    let p_hp_post2 = *app.world().get::<Health>(player).unwrap();
    assert_eq!(p_hp_post, p_hp_post2, "No mutations allowed after unload");
}

#[test]
fn test_adversarial_example_2_query_filter_isolation() {
    let dylib = get_example_2_dylib();
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Health>();
    app.register_mod_component::<PlayerTag>();
    app.register_mod_component::<DroneTag>();

    // Entity A: Has Health, Position, and PlayerTag.
    // FleetLifecycleSystem has WITHOUT PlayerTag -> MUST IGNORE Entity A.
    // PlayerRegenSystem has FILTER_ONLY PlayerTag -> MUST HEAL Entity A.
    let e_player = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Health {
                current: 50.0,
                max: 100.0,
            },
            PlayerTag { id: 99 },
        ))
        .id();

    // Entity B: Has Health, Position, and DroneTag (NO PlayerTag).
    // FleetLifecycleSystem has WITHOUT PlayerTag -> MUST DAMAGE Entity B (-10 HP).
    // PlayerRegenSystem has FILTER_ONLY PlayerTag -> MUST NOT HEAL Entity B.
    let e_drone = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            Health {
                current: 50.0,
                max: 100.0,
            },
            DroneTag { squad: 99 },
        ))
        .id();

    // Flush initial spawn
    app.update();

    let mod_id = app.load_mod_tracked(&dylib).expect("load mod");

    // Run 1 tick
    app.update();

    let h_player = *app.world().get::<Health>(e_player).unwrap();
    let h_drone = *app.world().get::<Health>(e_drone).unwrap();

    // Verify filter isolation:
    // Player MUST be healed +5 (50 -> 55), NOT damaged (-10)
    assert_eq!(
        h_player.current, 55.0,
        "Player should strictly receive healing, not attrition damage"
    );

    // Drone MUST be damaged -10 (50 -> 40), NOT healed (+5)
    assert_eq!(
        h_drone.current, 40.0,
        "Drone should strictly receive attrition damage, not player healing"
    );

    app.unload_mod(mod_id).expect("unload mod");
}

// ==============================================================================
// CHALLENGE 3: EXAMPLE 3 C99 + RUST CONCURRENCY & MEMORY ISOLATION
// ==============================================================================

#[test]
fn test_adversarial_example_3_c99_rust_concurrency_and_memory_isolation() {
    let (c_dylib, rust_dylib) = get_example_3_dylibs();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<ColorRgba>();

    // Spawn 100 drones with initial positions and velocities
    let mut drones = Vec::with_capacity(100);
    for i in 0..100 {
        let vx = ((i % 10) as f32) - 5.0; // [-5.0, 4.0]
        let vy = (((i / 10) % 10) as f32) - 5.0; // [-5.0, 4.0]
        let vz = 0.0;
        let d = app
            .world_mut()
            .spawn((
                Position {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: vx,
                    y: vy,
                    z: vz,
                },
                ColorRgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            ))
            .id();
        drones.push((d, vx, vy, vz));
    }

    // Flush initial spawn
    app.update();

    // Load both mods concurrently
    let c_mod_id = app.load_mod_tracked(&c_dylib).expect("load C99 mod");
    let rust_mod_id = app.load_mod_tracked(&rust_dylib).expect("load Rust mod");

    // Run 20 concurrent ticks
    for _ in 1..=20 {
        app.update();
    }

    // Verify invariants on all 100 drones:
    for (i, &(entity, vx, vy, vz)) in drones.iter().enumerate() {
        let pos = *app.world().get::<Position>(entity).unwrap();
        let color = *app.world().get::<ColorRgba>(entity).unwrap();

        // 1. C99 Kinematics invariant: pos = init + 20 * vel * 0.1
        let expected_x = 20.0 * vx * 0.1;
        let expected_y = 20.0 * vy * 0.1;
        let expected_z = 20.0 * vz * 0.1;
        assert!(
            (pos.x - expected_x).abs() < 1e-3,
            "Drone {} X mismatch: expected {}, got {}",
            i,
            expected_x,
            pos.x
        );
        assert!(
            (pos.y - expected_y).abs() < 1e-3,
            "Drone {} Y mismatch: expected {}, got {}",
            i,
            expected_y,
            pos.y
        );
        assert!(
            (pos.z - expected_z).abs() < 1e-3,
            "Drone {} Z mismatch: expected {}, got {}",
            i,
            expected_z,
            pos.z
        );

        // 2. Rust Color invariant: Color derives from Position
        // Depending on scheduler order within the tick, color is computed either from
        // the current or previous tick position, strictly within [0.1, 1.0].
        assert!(
            color.r >= 0.1 && color.r <= 1.0,
            "Drone {} R out of bounds: {}",
            i,
            color.r
        );
        assert!(
            color.g >= 0.2 && color.g <= 1.0,
            "Drone {} G out of bounds: {}",
            i,
            color.g
        );
        assert!(
            color.b >= 0.3 && color.b <= 1.0,
            "Drone {} B out of bounds: {}",
            i,
            color.b
        );
        assert_eq!(color.a, 1.0, "Drone {} Alpha must remain 1.0", i);

        // 3. Memory isolation: no NaN, Inf, or out-of-range memory stomping
        assert!(!pos.x.is_nan() && !pos.x.is_infinite());
        assert!(!pos.y.is_nan() && !pos.y.is_infinite());
        assert!(!pos.z.is_nan() && !pos.z.is_infinite());
    }

    // Asymmetric unload test: Unload C99 mod first!
    app.unload_mod(c_mod_id).expect("unload C99 mod");

    // Capture frozen positions
    let frozen_positions: Vec<Position> = drones
        .iter()
        .map(|&(e, ..)| *app.world().get::<Position>(e).unwrap())
        .collect();

    // Run 5 ticks with only Rust mod active
    for _ in 0..5 {
        app.update();
    }

    // Assert that positions remained 100% frozen after C99 unload!
    let positions_after_c_unload: Vec<Position> = drones
        .iter()
        .map(|&(e, ..)| *app.world().get::<Position>(e).unwrap())
        .collect();
    assert_eq!(
        frozen_positions, positions_after_c_unload,
        "Positions must remain completely frozen after C99 mod unload"
    );

    // Unload Rust mod as well
    app.unload_mod(rust_mod_id).expect("unload Rust mod");

    // Capture frozen state for both Position and Color
    let frozen_colors: Vec<ColorRgba> = drones
        .iter()
        .map(|&(e, ..)| *app.world().get::<ColorRgba>(e).unwrap())
        .collect();

    // Run 5 post-unload ticks with both unloaded
    for _ in 0..5 {
        app.update();
    }

    let final_positions: Vec<Position> = drones
        .iter()
        .map(|&(e, ..)| *app.world().get::<Position>(e).unwrap())
        .collect();
    let final_colors: Vec<ColorRgba> = drones
        .iter()
        .map(|&(e, ..)| *app.world().get::<ColorRgba>(e).unwrap())
        .collect();

    assert_eq!(
        frozen_positions, final_positions,
        "Zero position drift after both unloaded"
    );
    assert_eq!(
        frozen_colors, final_colors,
        "Zero color drift after both unloaded"
    );
}
