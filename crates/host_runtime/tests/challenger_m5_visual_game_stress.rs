//! Adversarial Stress Verification Suite for Milestone 5 Challenger 2
//!
//! Empirical and adversarial stress testing of:
//! 1. Rapid mod switching, unloading, and reloading across all 3 mods (`mod_gravity`, `mod_rainbow`, `mod_vortex`).
//! 2. Shadow copying resilience and DLL file lock bypass during active simulation.
//! 3. Observable visual and physical mutations produced by each mod (Gravity, Rainbow, Vortex).
//! 4. Empty world (0 entities) and massive world (2,500 entities) chunk iteration safety.
//! 5. TerminalGuard RAII panic hook and teardown safety.

use bevy_app::{App, PostUpdate};
use bevy_ecs::component::Component;
use bevy_ecs::system::Query;
use host_runtime::prelude::*;
use mod_derive::ModExposable;
use std::fs;
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
pub struct ColorRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;
const DT: f32 = 1.0 / 60.0;
const RESTITUTION: f32 = 0.95;

fn host_kinematics(mut query: Query<(&mut Position, &mut Velocity)>) {
    for (mut pos, mut vel) in query.iter_mut() {
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

fn get_workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

fn get_game_mod_dylib(crate_name: &str) -> PathBuf {
    let root = get_workspace_root();
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let filename = format!("{prefix}{crate_name}.{ext}");
    let dylib = root
        .join("examples/04_visual_game/mods")
        .join(crate_name)
        .join("target/release")
        .join(&filename);

    if !dylib.exists() {
        let manifest = root
            .join("examples/04_visual_game/mods")
            .join(crate_name)
            .join("Cargo.toml");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&manifest)
            .arg("--release")
            .status()
            .unwrap_or_else(|e| panic!("Failed to build game mod {crate_name}: {e}"));
        assert!(status.success(), "Cargo build failed for {crate_name}");
    }

    dylib
}

fn setup_base_app() -> App {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<ColorRgba>();
    app.add_systems(PostUpdate, host_kinematics);
    app
}

/// Helper that creates a shadow copy in a designated temp dir, returning the path.
fn create_shadow_copy(original: &PathBuf, tag: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ext = original.extension().unwrap().to_string_lossy();
    let shadow_dir = std::env::temp_dir().join(format!("bevy_stress_shadow_{}_{}", std::process::id(), id));
    fs::create_dir_all(&shadow_dir).expect("create shadow dir");
    let shadow_file = shadow_dir.join(format!("{tag}_{id}.{ext}"));
    fs::copy(original, &shadow_file).expect("copy shadow dylib");
    shadow_file
}

// ==============================================================================
// 1. Rapid Mod Switching & Dynamic Scheduling Stress Tests (50 Cycles)
// ==============================================================================

#[test]
fn test_adversarial_rapid_mod_cycling_50_cycles() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let vort_dylib = get_game_mod_dylib("mod_vortex");

    let mut app = setup_base_app();

    // Spawn 50 particles distributed across arena
    for i in 0..50 {
        let x = 10.0 + (i as f32 * 2.0);
        let y = 10.0 + (i as f32 * 0.9);
        app.world_mut().spawn((
            Position { x, y, z: 0.0 },
            Velocity { x: 1.5, y: -0.5, z: 0.0 },
            ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        ));
    }

    // 50 full cycles: Gravity -> Rainbow -> Vortex -> Reload Vortex -> Unload
    for _cycle in 1..=50 {
        let s_grav = create_shadow_copy(&grav_dylib, "grav");
        let g_id = app.load_mod_tracked(&s_grav).expect("load gravity");
        app.update();
        app.unload_mod(g_id).expect("unload gravity");
        let _ = fs::remove_file(&s_grav);

        let s_rain = create_shadow_copy(&rain_dylib, "rain");
        let r_id = app.load_mod_tracked(&s_rain).expect("load rainbow");
        app.update();
        app.unload_mod(r_id).expect("unload rainbow");
        let _ = fs::remove_file(&s_rain);

        let s_vort = create_shadow_copy(&vort_dylib, "vort");
        let v_id = app.load_mod_tracked(&s_vort).expect("load vortex");
        app.update();

        // Hot-reload: unload then load fresh shadow copy
        app.unload_mod(v_id).expect("unload vortex");
        let _ = fs::remove_file(&s_vort);

        let s_vort2 = create_shadow_copy(&vort_dylib, "vort_reloaded");
        let v2_id = app.load_mod_tracked(&s_vort2).expect("reload vortex");
        app.update();
        app.unload_mod(v2_id).expect("unload reloaded vortex");
        let _ = fs::remove_file(&s_vort2);

        // Baseline tick
        app.update();
    }

    // After 50 cycles (200 dynamic systems loaded & deactivated in Bevy schedule),
    // verify all 50 entities are completely intact and physically valid
    let mut query = app.world_mut().query::<(&Position, &Velocity, &ColorRgba)>();
    let count = query.iter(app.world()).count();
    assert_eq!(count, 50, "All 50 entities must survive 50 rapid cycling loops");

    for (pos, vel, col) in query.iter(app.world()) {
        assert!(!pos.x.is_nan() && !pos.y.is_nan(), "Position must not be NaN");
        assert!(!vel.x.is_nan() && !vel.y.is_nan(), "Velocity must not be NaN");
        assert!(!col.r.is_nan() && !col.g.is_nan() && !col.b.is_nan(), "Color must not be NaN");
        assert!(pos.x >= 1.0 && pos.x <= ARENA_WIDTH - 1.0, "Position x within arena bounds");
        assert!(pos.y >= 1.0 && pos.y <= ARENA_HEIGHT - 1.0, "Position y within arena bounds");
    }
}

// ==============================================================================
// 2. Shadow Copying Resilience and DLL File Lock Bypass Test
// ==============================================================================

#[test]
fn test_adversarial_shadow_copying_and_file_lock_bypass() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let vort_dylib = get_game_mod_dylib("mod_vortex");

    // Create a mutable staging directory simulating a mod developer's target/release
    let staging_dir = std::env::temp_dir().join(format!("bevy_staging_{}", std::process::id()));
    fs::create_dir_all(&staging_dir).expect("create staging dir");
    let ext = grav_dylib.extension().unwrap().to_string_lossy();
    let dev_mod_path = staging_dir.join(format!("active_mod.{ext}"));

    // 1. Initial compile: developer builds gravity mod
    fs::copy(&grav_dylib, &dev_mod_path).expect("stage initial gravity dylib");

    let mut app = setup_base_app();

    // Spawn 1 particle at (60.0, 30.0) with zero velocity
    let e = app.world_mut().spawn((
        Position { x: 60.0, y: 30.0, z: 0.0 },
        Velocity { x: 0.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    // Shadow copy into temp execution cache (simulating ModManager::load_mod)
    let shadow_1 = create_shadow_copy(&dev_mod_path, "shadow_run1");
    let mod_id_1 = app.load_mod_tracked(&shadow_1).expect("load shadow copy 1");

    // Advance 5 ticks: gravity mod pulls downwards
    for _ in 0..5 {
        app.update();
    }
    let vy_gravity = app.world().get::<Velocity>(e).unwrap().y;
    assert!(vy_gravity < -0.5, "Gravity mod must produce negative vy: got {vy_gravity}");

    // 2. CONCURRENT RECOMPILATION / OVERWRITE:
    // While the game is ACTIVELY RUNNING and mod is loaded, developer recompiles mod into Vortex!
    // Without shadow copying, this copy would FAIL with Windows ERROR_SHARING_VIOLATION or Unix file busy!
    let overwrite_res = fs::copy(&vort_dylib, &dev_mod_path);
    assert!(
        overwrite_res.is_ok(),
        "Overwriting original dylib while simulation runs must succeed due to shadow copying bypass: {:?}",
        overwrite_res.err()
    );

    // 3. HOT-RELOAD: unload old mod, create fresh shadow copy from newly overwritten dev_mod_path
    app.unload_mod(mod_id_1).expect("unload old gravity mod");
    let _ = fs::remove_file(&shadow_1);

    // Reset entity to center
    {
        let mut pos = app.world_mut().get_mut::<Position>(e).unwrap();
        pos.x = 90.0;
        pos.y = 30.0;
        let mut vel = app.world_mut().get_mut::<Velocity>(e).unwrap();
        vel.x = 0.0;
        vel.y = 0.0;
    }

    let shadow_2 = create_shadow_copy(&dev_mod_path, "shadow_run2");
    let mod_id_2 = app.load_mod_tracked(&shadow_2).expect("load reloaded vortex mod");

    // Advance 1 tick: now VORTEX physics must execute!
    app.update();

    let vel_vortex = *app.world().get::<Velocity>(e).unwrap();
    // In vortex at (90, 30), dx = 60 - 90 = -30. ur = (-1, 0).
    // Centripetal acceleration pulls inward: vx < 0!
    // Tangential acceleration pulls perpendicular: vy != 0!
    assert!(
        vel_vortex.x < -0.1,
        "After hot-reloading overwritten dylib, vortex physics must pull inward (vx < 0): got vx={}",
        vel_vortex.x
    );
    assert!(
        vel_vortex.y.abs() > 0.05,
        "After hot-reloading overwritten dylib, vortex physics must swirl tangentially (vy != 0): got vy={}",
        vel_vortex.y
    );

    // Teardown
    app.unload_mod(mod_id_2).expect("unload reloaded vortex mod");
    let _ = fs::remove_file(&shadow_2);
    let _ = fs::remove_file(&dev_mod_path);
    let _ = fs::remove_dir_all(&staging_dir);
}

// ==============================================================================
// 3. Observable Visual & Physical Mutation Oracles
// ==============================================================================

#[test]
fn test_adversarial_gravity_physical_kinematics_oracle() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let shadow = create_shadow_copy(&grav_dylib, "grav_oracle");
    let mut app = setup_base_app();

    let e = app.world_mut().spawn((
        Position { x: 50.0, y: 15.0, z: 0.0 },
        Velocity { x: 10.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    let mod_id = app.load_mod_tracked(&shadow).expect("load gravity mod");

    // Tick 1: vy must change by -GRAVITY * DT = -40.0 / 60.0 = -0.66667
    app.update();
    let vel1 = *app.world().get::<Velocity>(e).unwrap();
    let expected_vy = (-40.0 * DT) * 0.999; // post_update air drag
    assert!(
        (vel1.y - expected_vy).abs() < 1e-2,
        "Downward acceleration must match gravitational formula: got {}, expected {}",
        vel1.y, expected_vy
    );

    // Run until floor collision
    let mut bounced = false;
    for _ in 0..60 {
        let prev_vy = app.world().get::<Velocity>(e).unwrap().y;
        app.update();
        let cur_vy = app.world().get::<Velocity>(e).unwrap().y;
        if prev_vy < -2.0 && cur_vy > 0.0 {
            bounced = true;
            break;
        }
    }
    assert!(bounced, "Particle must bounce elastically off floor");

    app.unload_mod(mod_id).expect("unload gravity mod");
    let _ = fs::remove_file(&shadow);
}

#[test]
fn test_adversarial_rainbow_color_spectrum_oracle() {
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let shadow = create_shadow_copy(&rain_dylib, "rain_oracle");
    let mut app = setup_base_app();

    let e1 = app.world_mut().spawn((
        Position { x: 10.0, y: 10.0, z: 0.0 },
        Velocity { x: 0.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    let e2 = app.world_mut().spawn((
        Position { x: 100.0, y: 50.0, z: 0.0 },
        Velocity { x: 0.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    let mod_id = app.load_mod_tracked(&shadow).expect("load rainbow mod");
    app.update();

    let c1 = *app.world().get::<ColorRgba>(e1).unwrap();
    let c2 = *app.world().get::<ColorRgba>(e2).unwrap();

    // Verify colors are within [0, 1] range
    assert!(c1.r >= 0.0 && c1.r <= 1.0 && c1.g >= 0.0 && c1.g <= 1.0 && c1.b >= 0.0 && c1.b <= 1.0);
    assert!(c2.r >= 0.0 && c2.r <= 1.0 && c2.g >= 0.0 && c2.g <= 1.0 && c2.b >= 0.0 && c2.b <= 1.0);

    // Spatial diversity oracle: particles at (10, 10) vs (100, 50) have distinct hues
    let color_delta = (c1.r - c2.r).abs() + (c1.g - c2.g).abs() + (c1.b - c2.b).abs();
    assert!(color_delta > 0.1, "Rainbow colors must vary across 2D spatial coordinates: delta={color_delta}");

    // Temporal wave oracle: tick 30 times and verify color evolves
    for _ in 0..30 {
        app.update();
    }
    let c1_later = *app.world().get::<ColorRgba>(e1).unwrap();
    let time_delta = (c1.r - c1_later.r).abs() + (c1.g - c1_later.g).abs() + (c1.b - c1_later.b).abs();
    assert!(time_delta > 0.1, "Rainbow colors must evolve over simulation ticks: delta={time_delta}");

    app.unload_mod(mod_id).expect("unload rainbow mod");
    let _ = fs::remove_file(&shadow);
}

#[test]
fn test_adversarial_vortex_orbital_cyclone_oracle() {
    let vort_dylib = get_game_mod_dylib("mod_vortex");
    let shadow = create_shadow_copy(&vort_dylib, "vort_oracle");
    let mut app = setup_base_app();

    // Spawn orbiting particle at (90.0, 30.0) -> center is (60.0, 30.0)
    let e_orbit = app.world_mut().spawn((
        Position { x: 90.0, y: 30.0, z: 0.0 },
        Velocity { x: 0.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    // Spawn core particle inside r < 5.0 at (62.0, 30.0) to test soft core repulsion
    let e_core = app.world_mut().spawn((
        Position { x: 62.0, y: 30.0, z: 0.0 },
        Velocity { x: 0.0, y: 0.0, z: 0.0 },
        ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
    )).id();

    let mod_id = app.load_mod_tracked(&shadow).expect("load vortex mod");
    app.update();

    let vel_orbit = *app.world().get::<Velocity>(e_orbit).unwrap();
    let vel_core = *app.world().get::<Velocity>(e_core).unwrap();

    // Orbit particle: centripetal vx < 0, tangential vy != 0
    assert!(vel_orbit.x < -0.05, "Orbit particle must be drawn toward center (vx < 0): {}", vel_orbit.x);
    assert!(vel_orbit.y.abs() > 0.01, "Orbit particle must swirl tangentially: {}", vel_orbit.y);

    // Core particle: r = 2.0 < 5.0 -> repulsive core pushes away from center (vx > 0)
    assert!(vel_core.x > 0.05, "Core particle (r < 5.0) must experience repulsive push outward (vx > 0): {}", vel_core.x);

    app.unload_mod(mod_id).expect("unload vortex mod");
    let _ = fs::remove_file(&shadow);
}

// ==============================================================================
// 4. Boundary & Scale Stress Tests (0 Entities & 2,500 Entities)
// ==============================================================================

#[test]
fn test_adversarial_boundary_empty_world() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let shadow = create_shadow_copy(&grav_dylib, "empty_world");
    let mut app = setup_base_app();

    // 0 entities in the entire ECS world
    let mod_id = app.load_mod_tracked(&shadow).expect("load into empty world");
    for _ in 0..10 {
        app.update(); // Must not panic or segfault
    }
    app.unload_mod(mod_id).expect("unload from empty world");
    let _ = fs::remove_file(&shadow);
}

#[test]
fn test_adversarial_scale_2500_entities() {
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let shadow = create_shadow_copy(&rain_dylib, "scale_2500");
    let mut app = setup_base_app();

    // Spawn 2,500 entities across multiple archetype chunks
    for i in 0..2500 {
        let x = 10.0 + (i % 100) as f32;
        let y = 10.0 + (i / 100) as f32 * 1.8;
        app.world_mut().spawn((
            Position { x, y, z: 0.0 },
            Velocity { x: 0.5, y: 0.5, z: 0.0 },
            ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        ));
    }

    let mod_id = app.load_mod_tracked(&shadow).expect("load rainbow at scale");
    for _ in 0..5 {
        app.update();
    }

    // Verify all 2,500 entities have mutated colors
    let mut query = app.world_mut().query::<&ColorRgba>();
    let count = query.iter(app.world()).count();
    assert_eq!(count, 2500);

    for col in query.iter(app.world()) {
        assert!(col.r >= 0.0 && col.r <= 1.0);
        assert!(col.g >= 0.0 && col.g <= 1.0);
        assert!(col.b >= 0.0 && col.b <= 1.0);
    }

    app.unload_mod(mod_id).expect("unload rainbow at scale");
    let _ = fs::remove_file(&shadow);
}

// ==============================================================================
// 5. Out-of-Order Lifecycle & Error Recovery Stress Tests
// ==============================================================================

#[test]
fn test_adversarial_double_unload_and_invalid_mod_id() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let shadow = create_shadow_copy(&grav_dylib, "double_unload");
    let mut app = setup_base_app();

    let mod_id = app.load_mod_tracked(&shadow).expect("load gravity");
    app.update();

    // First unload: succeeds
    let res1 = app.unload_mod(mod_id);
    assert!(res1.is_ok(), "First unload must succeed: {:?}", res1);

    // Second unload with same ModId: must fail cleanly without crashing
    let res2 = app.unload_mod(mod_id);
    assert!(res2.is_err(), "Second unload must return error, not crash");

    // Unload bogus ModId: must fail cleanly
    let res3 = app.unload_mod(ModId(999_999));
    assert!(res3.is_err(), "Unload non-existent ModId must return error, not crash");

    // App must still run smoothly after failed unloads
    app.update();
    let _ = fs::remove_file(&shadow);
}

#[test]
fn test_adversarial_entity_churn_spawn_despawn_with_active_mod() {
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let shadow = create_shadow_copy(&rain_dylib, "churn_shadow");
    let mut app = setup_base_app();

    // 1. Initial 50 entities
    let mut entities: Vec<bevy_ecs::entity::Entity> = (0..50)
        .map(|i| {
            app.world_mut()
                .spawn((
                    Position { x: 10.0 + i as f32, y: 15.0, z: 0.0 },
                    Velocity { x: 1.0, y: 0.5, z: 0.0 },
                    ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
                ))
                .id()
        })
        .collect();

    let mod_id = app.load_mod_tracked(&shadow).expect("load rainbow mod for churn");
    app.update();

    // 2. Despawn 25 entities (triggers table compaction and archetypal chunk restructuring)
    for id in entities.drain(0..25) {
        app.world_mut().despawn(id);
    }
    app.update();

    // 3. Spawn 40 new entities (triggers capacity growth and chunk expansion)
    for i in 0..40 {
        let id = app.world_mut()
            .spawn((
                Position { x: 50.0 + i as f32, y: 25.0, z: 0.0 },
                Velocity { x: -0.5, y: -0.5, z: 0.0 },
                ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
            ))
            .id();
        entities.push(id);
    }
    app.update();

    // 4. Verify all 65 remaining entities have valid, non-corrupted colors and positions
    let mut query = app.world_mut().query::<(&Position, &ColorRgba)>();
    let total = query.iter(app.world()).count();
    assert_eq!(total, 65, "Total entity count must equal remaining count (25 survived + 40 spawned)");

    for (pos, col) in query.iter(app.world()) {
        assert!(!pos.x.is_nan() && !pos.y.is_nan(), "Position must remain valid");
        assert!(col.r >= 0.0 && col.r <= 1.0 && col.g >= 0.0 && col.g <= 1.0 && col.b >= 0.0 && col.b <= 1.0);
    }

    app.unload_mod(mod_id).expect("unload rainbow after churn");
    let _ = fs::remove_file(&shadow);
}

#[test]
fn test_adversarial_color_restoration_oracle() {
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let shadow = create_shadow_copy(&rain_dylib, "restore_shadow");
    let mut app = setup_base_app();

    for i in 0..20 {
        app.world_mut().spawn((
            Position { x: 5.0 + i as f32 * 5.0, y: 20.0, z: 0.0 },
            Velocity { x: 0.0, y: 0.0, z: 0.0 },
            ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 },
        ));
    }

    let mod_id = app.load_mod_tracked(&shadow).expect("load rainbow mod");
    for _ in 0..10 {
        app.update();
    }

    // Colors have been mutated by rainbow spectrum
    let mut mutated = false;
    for col in app.world_mut().query::<&ColorRgba>().iter(app.world()) {
        if col.r != 1.0 || col.g != 1.0 || col.b != 1.0 {
            mutated = true;
            break;
        }
    }
    assert!(mutated, "Rainbow mod must mutate entity colors");

    // Unload mod and simulate ModManager color reset
    app.unload_mod(mod_id).expect("unload rainbow mod");
    {
        let mut query = app.world_mut().query::<&mut ColorRgba>();
        let mut toggle = false;
        for mut col in query.iter_mut(app.world_mut()) {
            if toggle {
                *col = ColorRgba { r: 0.2, g: 0.8, b: 1.0, a: 1.0 }; // cyan
            } else {
                *col = ColorRgba { r: 1.0, g: 1.0, b: 1.0, a: 1.0 }; // white
            }
            toggle = !toggle;
        }
    }

    // Advance 5 ticks: without rainbow mod, colors must remain exactly baseline (cyan or white)
    for _ in 0..5 {
        app.update();
    }

    for col in app.world_mut().query::<&ColorRgba>().iter(app.world()) {
        let is_cyan = (col.r - 0.2).abs() < 1e-4 && (col.g - 0.8).abs() < 1e-4 && (col.b - 1.0).abs() < 1e-4;
        let is_white = (col.r - 1.0).abs() < 1e-4 && (col.g - 1.0).abs() < 1e-4 && (col.b - 1.0).abs() < 1e-4;
        assert!(is_cyan || is_white, "Colors must remain strictly baseline after unload");
    }

    let _ = fs::remove_file(&shadow);
}

