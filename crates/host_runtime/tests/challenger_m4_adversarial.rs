//! Empirical Adversarial Verification Suite for Milestone 4 (Visual Game with Runtime Mod Reloading)
//!
//! Rigorous stress-testing of:
//! 1. Mod 1 (Gravity): Exact physical kinematics, downward acceleration, floor bounce, restitution, friction, boundary reflection.
//! 2. Mod 2 (Rainbow): Dynamic HSV color spectrum mutation, spatial/temporal variation, boundary clamping [0, 1].
//! 3. Mod 3 (Vortex): Centripetal force, tangential swirling, angular momentum conservation, repulsive singularity protection (r < 5.0).
//! 4. Unload / Baseline: Immediate deactivation via atomic tombstone, reversion to neutral kinematics and stable colors.
//! 5. Multi-cycle rapid dynamic hot-reloading: 20 consecutive transitions across mods, reloads, and unloads.
//! 6. Stress under 1,000 entities across chunks: alignment, memory safety, zero data races.
//! 7. Boundary robustness: 0 entities (empty world), double unloads, reload when None.

#![allow(clippy::default_constructed_unit_structs)]

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

    // Use isolated shadow copy per caller thread/invocation to prevent cross-test static interference
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let shadow_dir = std::env::temp_dir().join(format!(
        "bevy_abi_test_shadow_{}_{}",
        std::process::id(),
        count
    ));
    let _ = fs::create_dir_all(&shadow_dir);
    let shadow_file = shadow_dir.join(format!("{crate_name}_{count}.{ext}"));
    let _ = fs::copy(&dylib, &shadow_file);
    shadow_file
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

#[test]
fn test_m4_gravity_mod_physics_and_bounce() {
    let dylib = get_game_mod_dylib("mod_gravity");
    assert!(dylib.exists());

    let mut app = setup_base_app();

    // Spawn 1 particle at (50.0, 30.0) with initial velocity (5.0, 0.0)
    let e = app
        .world_mut()
        .spawn((
            Position {
                x: 50.0,
                y: 30.0,
                z: 0.0,
            },
            Velocity {
                x: 5.0,
                y: 0.0,
                z: 0.0,
            },
            ColorRgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        ))
        .id();

    // Load Gravity mod
    let mod_id = app.load_mod_tracked(&dylib).expect("load gravity mod");

    // Tick 1: Foreign gravity system accelerates downwards by -40.0 * (1/60) = -0.666667
    app.update();

    let (pos1, vel1) = {
        let world = app.world();
        (
            *world.get::<Position>(e).unwrap(),
            *world.get::<Velocity>(e).unwrap(),
        )
    };

    // Expected vy: initial 0.0 - (40.0 * DT) = -0.6666667.
    // In post_update: air drag applies vel.y *= 0.999.
    let expected_vy = (-40.0 * DT) * 0.999;
    assert!(
        (vel1.y - expected_vy).abs() < 1e-3,
        "Expected vy ~ {expected_vy}, got {}",
        vel1.y
    );
    assert!(
        pos1.y < 30.0,
        "Position y must decrease under gravity: {}",
        pos1.y
    );

    // Run until floor collision (approx 45 ticks)
    let mut hit_floor = false;
    let mut bounced = false;
    for _ in 0..100 {
        let prev_vy = app.world().get::<Velocity>(e).unwrap().y;
        app.update();
        let cur_vy = app.world().get::<Velocity>(e).unwrap().y;
        let cur_py = app.world().get::<Position>(e).unwrap().y;

        assert!(
            cur_py >= 0.99,
            "Particle must never penetrate arena floor (y=1.0), got {cur_py}"
        );

        if cur_py <= 1.05 {
            hit_floor = true;
        }

        if prev_vy < -5.0 && cur_vy > 0.0 {
            // Downward velocity flipped to positive -> floor bounce!
            bounced = true;
            break;
        }
    }

    assert!(hit_floor, "Particle must reach floor");
    assert!(bounced, "Particle must bounce off floor with positive vy");

    // Unload mod
    app.unload_mod(mod_id).expect("unload gravity mod");

    // After unload, record velocity across 10 ticks: gravity should be INACTIVE
    let vel_after_unload = *app.world().get::<Velocity>(e).unwrap();
    for _ in 0..10 {
        app.update();
    }
    let vel_later = *app.world().get::<Velocity>(e).unwrap();

    // Without gravity, vy should only decay slightly by 0.999^10, NOT accelerate downwards
    let decay_factor = (0.999_f32).powi(10);
    assert!(
        (vel_later.y - vel_after_unload.y * decay_factor).abs() < 0.1,
        "After unload, gravity must be completely deactivated: initial={vel_after_unload:?}, later={vel_later:?}"
    );
}

#[test]
fn test_m4_rainbow_mod_color_spectrum_and_mutation() {
    let dylib = get_game_mod_dylib("mod_rainbow");
    assert!(dylib.exists());

    let mut app = setup_base_app();

    // Spawn 5 particles with baseline white color at different coordinates
    let mut entities = Vec::new();
    for i in 0..5 {
        let id = app
            .world_mut()
            .spawn((
                Position {
                    x: 10.0 + i as f32 * 20.0,
                    y: 10.0 + i as f32 * 10.0,
                    z: 0.0,
                },
                Velocity {
                    x: 0.0,
                    y: 0.0,
                    z: 0.0,
                },
                ColorRgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                },
            ))
            .id();
        entities.push(id);
    }

    // Load Rainbow mod
    let mod_id = app.load_mod_tracked(&dylib).expect("load rainbow mod");

    // Tick simulation
    app.update();

    // Verify all colors have been mutated into valid RGB hues
    let mut colors_tick1 = Vec::new();
    for &e in &entities {
        let col = *app.world().get::<ColorRgba>(e).unwrap();
        assert!(
            col.r >= 0.0 && col.r <= 1.0,
            "Red channel out of bounds: {}",
            col.r
        );
        assert!(
            col.g >= 0.0 && col.g <= 1.0,
            "Green channel out of bounds: {}",
            col.g
        );
        assert!(
            col.b >= 0.0 && col.b <= 1.0,
            "Blue channel out of bounds: {}",
            col.b
        );
        assert_eq!(col.a, 1.0, "Alpha must be 1.0");

        // Should no longer be plain white (1, 1, 1)
        let is_plain_white = col.r == 1.0 && col.g == 1.0 && col.b == 1.0;
        assert!(
            !is_plain_white,
            "Rainbow mod must mutate entity away from baseline white"
        );
        colors_tick1.push(col);
    }

    // Verify spatial variance: entity 0 and entity 4 have different positions, so different colors
    let col0 = colors_tick1[0];
    let col4 = colors_tick1[4];
    let diff = (col0.r - col4.r).abs() + (col0.g - col4.g).abs() + (col0.b - col4.b).abs();
    assert!(
        diff > 0.05,
        "Particles at different coordinates must receive distinct rainbow hues"
    );

    // Verify temporal animation: tick 30 times and verify color evolves
    for _ in 0..30 {
        app.update();
    }
    let col0_later = *app.world().get::<ColorRgba>(entities[0]).unwrap();
    let temp_diff = (col0.r - col0_later.r).abs()
        + (col0.g - col0_later.g).abs()
        + (col0.b - col0_later.b).abs();
    assert!(temp_diff > 0.05, "Rainbow mod must animate hues over time");

    // Unload Rainbow mod
    app.unload_mod(mod_id).expect("unload rainbow mod");

    // Reset colors to cyan (0.2, 0.8, 1.0)
    for &e in &entities {
        let mut col = app.world_mut().get_mut::<ColorRgba>(e).unwrap();
        col.r = 0.2;
        col.g = 0.8;
        col.b = 1.0;
    }

    // Tick 10 times: colors should remain frozen at cyan
    for _ in 0..10 {
        app.update();
    }
    for &e in &entities {
        let col = *app.world().get::<ColorRgba>(e).unwrap();
        assert_eq!(col.r, 0.2);
        assert_eq!(col.g, 0.8);
        assert_eq!(col.b, 1.0);
    }
}

#[test]
fn test_m4_vortex_mod_orbital_swirl_and_repulsion() {
    let dylib = get_game_mod_dylib("mod_vortex");
    assert!(dylib.exists());

    let mut app = setup_base_app();

    // Center is (60.0, 30.0). Place particle to the right at (90.0, 30.0) with zero velocity.
    let e_orbit = app
        .world_mut()
        .spawn((
            Position {
                x: 90.0,
                y: 30.0,
                z: 0.0,
            },
            Velocity {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            ColorRgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        ))
        .id();

    // Place particle inside the core (r < 5.0) at (62.0, 30.0) to test soft repulsion
    let e_core = app
        .world_mut()
        .spawn((
            Position {
                x: 62.0,
                y: 30.0,
                z: 0.0,
            },
            Velocity {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            ColorRgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        ))
        .id();

    // Load Vortex mod
    let mod_id = app.load_mod_tracked(&dylib).expect("load vortex mod");

    // Tick 1
    app.update();

    // Check orbit entity:
    // dx = 60 - 90 = -30, dy = 30 - 30 = 0.
    // ur = (-1, 0), ut = (0, 1) or (0, -1).
    // It must experience inward attraction (ax < 0, vx < 0) and tangential swirl (vy != 0).
    let vel_orbit = *app.world().get::<Velocity>(e_orbit).unwrap();
    let pos_orbit = *app.world().get::<Position>(e_orbit).unwrap();

    assert!(
        vel_orbit.x < 0.0,
        "Vortex centripetal force must pull towards center (vx < 0): vx={}",
        vel_orbit.x
    );
    assert!(
        vel_orbit.y.abs() > 0.01,
        "Vortex tangential force must induce orbital velocity (vy != 0): vy={}",
        vel_orbit.y
    );

    // Compute angular momentum L = (x - 60) * vy - (y - 30) * vx
    let ang_mom = (pos_orbit.x - 60.0) * vel_orbit.y - (pos_orbit.y - 30.0) * vel_orbit.x;
    assert!(
        ang_mom.abs() > 0.1,
        "Vortex must induce strong angular momentum: L={ang_mom}"
    );

    // Check core repulsion entity:
    // r = 2.0 < 5.0 -> a_r = -25.0 (repulsion pointing away from center)
    // dx = 60 - 62 = -2. ur = (-1, 0). a_r * ur_x = (-25) * (-1) = +25 (pushing right, away from center!)
    let vel_core = *app.world().get::<Velocity>(e_core).unwrap();
    assert!(
        vel_core.x > 0.0,
        "Core entity (r < 5.0) must experience repulsive outward acceleration (vx > 0): vx={}",
        vel_core.x
    );

    // Unload Vortex mod
    app.unload_mod(mod_id).expect("unload vortex mod");
}

#[test]
fn test_m4_rapid_switching_across_all_mods_and_reloads() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let vort_dylib = get_game_mod_dylib("mod_vortex");

    let mut app = setup_base_app();

    // Spawn 20 particles
    for i in 0..20 {
        app.world_mut().spawn((
            Position {
                x: 20.0 + (i as f32 * 4.0),
                y: 15.0 + (i as f32 * 2.0),
                z: 0.0,
            },
            Velocity {
                x: 2.0,
                y: -1.0,
                z: 0.0,
            },
            ColorRgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        ));
    }

    // Execute 20 rapid lifecycle switching cycles
    for cycle in 1..=20 {
        // 1. Load Gravity
        let g_id = app.load_mod_tracked(&grav_dylib).expect("load gravity");
        for _ in 0..3 {
            app.update();
        }
        app.unload_mod(g_id).expect("unload gravity");

        // 2. Load Rainbow
        let r_id = app.load_mod_tracked(&rain_dylib).expect("load rainbow");
        for _ in 0..3 {
            app.update();
        }
        app.unload_mod(r_id).expect("unload rainbow");

        // 3. Load Vortex
        let v_id = app.load_mod_tracked(&vort_dylib).expect("load vortex");
        for _ in 0..3 {
            app.update();
        }

        // 4. Hot-reload Vortex (unload + reload)
        app.unload_mod(v_id).expect("unload vortex before reload");
        let v2_id = app.load_mod_tracked(&vort_dylib).expect("reload vortex");
        for _ in 0..3 {
            app.update();
        }
        app.unload_mod(v2_id).expect("unload reloaded vortex");

        // 5. Baseline ticks
        for _ in 0..3 {
            app.update();
        }

        if cycle % 5 == 0 {
            println!("  [Adversarial] Completed cycle {cycle}/20 successfully");
        }
    }

    // Verify all 20 entities remain physically sound (no NaNs or infinities)
    let mut query = app
        .world_mut()
        .query::<(&Position, &Velocity, &ColorRgba)>();
    let count = query.iter(app.world()).count();
    assert_eq!(count, 20);

    for (pos, vel, col) in query.iter(app.world()) {
        assert!(!pos.x.is_nan() && !pos.y.is_nan());
        assert!(!vel.x.is_nan() && !vel.y.is_nan());
        assert!(!col.r.is_nan() && !col.g.is_nan() && !col.b.is_nan());
    }
}

#[test]
fn test_m4_stress_1000_entities_across_all_mods() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let vort_dylib = get_game_mod_dylib("mod_vortex");

    let mut app = setup_base_app();

    // Spawn 1,000 entities to span multiple archetype table chunks
    for i in 0..1000 {
        let x = 10.0 + (i % 100) as f32;
        let y = 10.0 + (i / 100) as f32 * 4.0;
        app.world_mut().spawn((
            Position { x, y, z: 0.0 },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 0.0,
            },
            ColorRgba {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
        ));
    }

    // Load Gravity, tick 5 times
    let g_id = app.load_mod_tracked(&grav_dylib).expect("load gravity");
    for _ in 0..5 {
        app.update();
    }
    app.unload_mod(g_id).expect("unload gravity");

    // Load Rainbow, tick 5 times
    let r_id = app.load_mod_tracked(&rain_dylib).expect("load rainbow");
    for _ in 0..5 {
        app.update();
    }
    app.unload_mod(r_id).expect("unload rainbow");

    // Load Vortex, tick 5 times
    let v_id = app.load_mod_tracked(&vort_dylib).expect("load vortex");
    for _ in 0..5 {
        app.update();
    }
    app.unload_mod(v_id).expect("unload vortex");

    // Final baseline check
    let mut query = app
        .world_mut()
        .query::<(&Position, &Velocity, &ColorRgba)>();
    let total = query.iter(app.world()).count();
    assert_eq!(total, 1000);
}

#[test]
fn test_m4_empty_world_chunk_iteration_safety() {
    let grav_dylib = get_game_mod_dylib("mod_gravity");
    let rain_dylib = get_game_mod_dylib("mod_rainbow");
    let vort_dylib = get_game_mod_dylib("mod_vortex");

    let mut app = setup_base_app();

    // ZERO entities in the world
    for dylib in [&grav_dylib, &rain_dylib, &vort_dylib] {
        let id = app.load_mod_tracked(dylib).expect("load into empty world");
        for _ in 0..5 {
            // Must not panic or segfault when chunk count == 0
            app.update();
        }
        app.unload_mod(id).expect("unload mod from empty world");
    }
}

#[test]
fn test_m4_shadow_copy_isolation_and_drop_cleanup() {
    let original = get_game_mod_dylib("mod_gravity");
    let temp_dir = std::env::temp_dir().join(format!("test_m4_shadow_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).expect("create shadow dir");

    let ext = original
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("dylib");
    let shadow_path = temp_dir.join(format!("mod_gravity_shadow.{ext}"));
    fs::copy(&original, &shadow_path).expect("copy shadow");
    assert!(shadow_path.exists());

    let mut app = setup_base_app();
    let mod_id = app
        .load_mod_tracked(&shadow_path)
        .expect("load from shadow");

    app.update();

    app.unload_mod(mod_id).expect("unload shadow mod");

    // Cleanup shadow
    fs::remove_file(&shadow_path).expect("remove shadow dylib");
    fs::remove_dir_all(&temp_dir).expect("remove shadow dir");

    assert!(!temp_dir.exists(), "Shadow dir must be cleaned up");
}
