use crate::support::fixture_dylib;
use bevy_abi_mod_host::prelude::*;
use bevy_app::{App, PostUpdate};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::system::Query;
use core::sync::atomic::{AtomicU64, Ordering};
use std::fs;

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Velocity")]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Health")]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::PlayerTag")]
pub struct PlayerTag {
    pub id: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::DroneTag")]
pub struct DroneTag {
    pub squad: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::ColorRgba")]
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

fn shadow_opts() -> LoadOptions {
    LoadOptions {
        shadow_copy: true,
        ..Default::default()
    }
}

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

fn setup_visual_game_app() -> App {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<ColorRgba>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.add_systems(PostUpdate, host_kinematics);
    app
}

// ==============================================================================
// 1. Minimal Handshake Pipeline (`minimal_guest_mod`)
// ==============================================================================

#[test]
fn test_minimal_handshake_repeated_lifecycle_reload() {
    let dylib = fixture_dylib("minimal_guest_mod");
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

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

    app.update();

    for cycle in 1..=15 {
        let mod_id = app
            .load_mod_with(&dylib, shadow_opts())
            .unwrap_or_else(|e| panic!("Failed to load mod in cycle {cycle}: {e:?}"));

        for _ in 0..3 {
            app.update();
        }

        app.unload_mod(mod_id)
            .unwrap_or_else(|e| panic!("Failed to unload mod in cycle {cycle}: {e:?}"));

        let pos_before_post_tick: Vec<Position> = entities
            .iter()
            .map(|&e| *app.world().get::<Position>(e).unwrap())
            .collect();

        app.update();

        let pos_after_post_tick: Vec<Position> = entities
            .iter()
            .map(|&e| *app.world().get::<Position>(e).unwrap())
            .collect();

        assert_eq!(
            pos_before_post_tick, pos_after_post_tick,
            "Cycle {cycle}: Post-unload tick must not modify Position"
        );
    }

    let e0_pos = *app.world().get::<Position>(entities[0]).unwrap();
    assert!((e0_pos.x - 45.0).abs() < 1e-3);
    assert!((e0_pos.y - 90.0).abs() < 1e-3);
    assert!((e0_pos.z - 135.0).abs() < 1e-3);
}

#[test]
fn test_minimal_handshake_empty_and_large_chunk_iteration() {
    let dylib = fixture_dylib("minimal_guest_mod");
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    let mod_id = app.load_mod_with(&dylib, shadow_opts()).expect("load mod");

    for _ in 0..5 {
        app.update();
    }

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

    for _ in 0..5 {
        app.update();
    }

    for (i, &e) in test_entities.iter().enumerate() {
        let pos = *app.world().get::<Position>(e).unwrap();
        assert!((pos.x - (i as f32 + 5.0)).abs() < 1e-3);
        assert!((pos.y - 10.0).abs() < 1e-3);
        assert!((pos.z - 15.0).abs() < 1e-3);
    }

    app.unload_mod(mod_id).expect("unload mod");
}

// ==============================================================================
// 2. Multi-System Pipeline & Deferred Commands (`example_02_guest_mod`)
// ==============================================================================

#[test]
fn test_multi_system_pipeline_deferred_commands_and_attrition() {
    let dylib = fixture_dylib("example_02_guest_mod");
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Health>(ComponentOptions::new().with_raw_insert().with_default())
        .unwrap();
    app.register_mod_component::<PlayerTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<DroneTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

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

    app.update();

    let mod_id = app
        .load_mod_with(&dylib, shadow_opts())
        .expect("load example 2 mod");
    app.update();

    for (i, &d) in low_hp_drones.iter().enumerate() {
        assert!(
            app.world().get_entity(d).is_err(),
            "Low-HP drone {i} ({d:?}) must be despawned after tick 1"
        );
    }

    for &d in &high_hp_drones {
        let h = app
            .world()
            .get::<Health>(d)
            .expect("high hp drone still exists");
        assert!((h.current - 90.0).abs() < 1e-3);
    }

    let p_hp = app.world().get::<Health>(player).unwrap();
    assert!((p_hp.current - 85.0).abs() < 1e-3);

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
    assert_eq!(reinforcement_count, 1);

    for _ in 2..=5 {
        app.update();
    }

    let p_hp_final = app.world().get::<Health>(player).unwrap();
    assert_eq!(p_hp_final.current, 100.0);

    for &d in &high_hp_drones {
        let h = app.world().get::<Health>(d).unwrap();
        assert!((h.current - 50.0).abs() < 1e-3);
    }

    app.unload_mod(mod_id).expect("unload example 2 mod");

    let p_hp_post = *app.world().get::<Health>(player).unwrap();
    app.update();
    let p_hp_post2 = *app.world().get::<Health>(player).unwrap();
    assert_eq!(p_hp_post, p_hp_post2);
}

#[test]
fn test_multi_system_pipeline_query_filter_isolation() {
    let dylib = fixture_dylib("example_02_guest_mod");
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Health>(ComponentOptions::new().with_raw_insert().with_default())
        .unwrap();
    app.register_mod_component::<PlayerTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<DroneTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

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

    app.update();

    let mod_id = app.load_mod_with(&dylib, shadow_opts()).expect("load mod");
    app.update();

    let h_player = *app.world().get::<Health>(e_player).unwrap();
    let h_drone = *app.world().get::<Health>(e_drone).unwrap();

    assert_eq!(h_player.current, 55.0);
    assert_eq!(h_drone.current, 40.0);

    app.unload_mod(mod_id).expect("unload mod");
}

// ==============================================================================
// 3. Cross-Language C99 + Rust Concurrent Pipeline (`example_03_*`)
// ==============================================================================

#[test]
fn test_cross_language_c99_and_rust_concurrency_and_asymmetric_unload() {
    let c_dylib = fixture_dylib("example_03_c_mod");
    let rust_dylib = fixture_dylib("example_03_rust_mod");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<ColorRgba>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    const DRONE_COUNT: usize = 500;
    let mut drones = Vec::with_capacity(DRONE_COUNT);
    for i in 0..DRONE_COUNT {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 5.0,
                    z: 0.0,
                },
                ColorRgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            ))
            .id();
        drones.push(e);
    }

    let c_mod_id = app
        .load_mod_with(&c_dylib, shadow_opts())
        .expect("Failed to load C99 mod");
    let rust_mod_id = app
        .load_mod_with(&rust_dylib, shadow_opts())
        .expect("Failed to load Rust mod");

    for _ in 0..50 {
        app.update();
    }

    let p0 = *app.world().get::<Position>(drones[0]).unwrap();
    let c0 = *app.world().get::<ColorRgba>(drones[0]).unwrap();

    assert!((p0.x - 50.0).abs() < 1e-3);
    assert!(c0.r > 0.0);
    assert!(!c0.r.is_nan() && !c0.r.is_infinite());

    app.unload_mod(c_mod_id).expect("Failed to unload C99 mod");

    let p0_snapshot = *app.world().get::<Position>(drones[0]).unwrap();
    let c0_snapshot = *app.world().get::<ColorRgba>(drones[0]).unwrap();

    for _ in 0..25 {
        app.update();
    }

    let p0_after_c_unload = *app.world().get::<Position>(drones[0]).unwrap();
    assert_eq!(p0_snapshot, p0_after_c_unload);

    app.world_mut().get_mut::<Position>(drones[0]).unwrap().x = 2.0;
    app.update();

    let c0_updated = *app.world().get::<ColorRgba>(drones[0]).unwrap();
    assert!((c0_updated.r - 0.2).abs() < 1e-3);
    assert_ne!(c0_snapshot.r, c0_updated.r);

    app.unload_mod(rust_mod_id)
        .expect("Failed to unload Rust mod");

    app.world_mut().get_mut::<Position>(drones[0]).unwrap().x = 15.0;
    app.update();

    let c0_final = *app.world().get::<ColorRgba>(drones[0]).unwrap();
    assert_eq!(c0_final, c0_updated);
}

// ==============================================================================
// 4. Visual Game Multi-Mod Switching & Core Shadow-Copy Oracles (`L-03`)
// ==============================================================================

#[test]
fn test_visual_game_rapid_mod_cycling_50_cycles() {
    let grav_dylib = fixture_dylib("mod_gravity");
    let rain_dylib = fixture_dylib("mod_rainbow");
    let vort_dylib = fixture_dylib("mod_vortex");

    let mut app = setup_visual_game_app();

    for i in 0..50 {
        let x = 10.0 + (i as f32 * 2.0);
        let y = 10.0 + (i as f32 * 0.9);
        app.world_mut().spawn((
            Position { x, y, z: 0.0 },
            Velocity {
                x: 1.5,
                y: -0.5,
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

    for _cycle in 1..=50 {
        let g_id = app
            .load_mod_with(&grav_dylib, shadow_opts())
            .expect("load gravity");
        app.update();
        app.unload_mod(g_id).expect("unload gravity");

        let r_id = app
            .load_mod_with(&rain_dylib, shadow_opts())
            .expect("load rainbow");
        app.update();
        app.unload_mod(r_id).expect("unload rainbow");

        let v_id = app
            .load_mod_with(&vort_dylib, shadow_opts())
            .expect("load vortex");
        app.update();
        app.unload_mod(v_id).expect("unload vortex");

        let v2_id = app
            .load_mod_with(&vort_dylib, shadow_opts())
            .expect("reload vortex");
        app.update();
        app.unload_mod(v2_id).expect("unload reloaded vortex");

        app.update();
    }

    let mut query = app
        .world_mut()
        .query::<(&Position, &Velocity, &ColorRgba)>();
    assert_eq!(query.iter(app.world()).count(), 50);

    for (pos, vel, col) in query.iter(app.world()) {
        assert!(!pos.x.is_nan() && !pos.y.is_nan());
        assert!(!vel.x.is_nan() && !vel.y.is_nan());
        assert!(!col.r.is_nan() && !col.g.is_nan() && !col.b.is_nan());
        assert!(pos.x >= 1.0 && pos.x <= ARENA_WIDTH - 1.0);
        assert!(pos.y >= 1.0 && pos.y <= ARENA_HEIGHT - 1.0);
    }
}

#[test]
fn test_visual_game_shadow_copying_and_file_lock_bypass() {
    static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);

    let grav_dylib = fixture_dylib("mod_gravity");
    let vort_dylib = fixture_dylib("mod_vortex");

    let staging_dir =
        std::env::temp_dir().join(format!("bevy_staging_{}_{}", std::process::id(), seq));
    fs::create_dir_all(&staging_dir).expect("create staging dir");
    let ext = grav_dylib.extension().unwrap().to_string_lossy();
    let dev_mod_path = staging_dir.join(format!("active_mod.{ext}"));

    fs::copy(&grav_dylib, &dev_mod_path).expect("stage initial gravity dylib");

    let mut app = setup_visual_game_app();

    let e = app
        .world_mut()
        .spawn((
            Position {
                x: 60.0,
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

    let mod_id_1 = app
        .load_mod_with(&dev_mod_path, shadow_opts())
        .expect("load core shadow copy 1");

    for _ in 0..5 {
        app.update();
    }
    let vy_gravity = app.world().get::<Velocity>(e).unwrap().y;
    assert!(vy_gravity < -0.5);

    // Overwrite `dev_mod_path` in place while `mod_id_1` is still loaded (`L-03`)
    let overwrite_res = fs::copy(&vort_dylib, &dev_mod_path);
    assert!(overwrite_res.is_ok());

    app.unload_mod(mod_id_1).expect("unload old gravity mod");

    {
        let mut pos = app.world_mut().get_mut::<Position>(e).unwrap();
        pos.x = 90.0;
        pos.y = 30.0;
        let mut vel = app.world_mut().get_mut::<Velocity>(e).unwrap();
        vel.x = 0.0;
        vel.y = 0.0;
    }

    let mod_id_2 = app
        .load_mod_with(&dev_mod_path, shadow_opts())
        .expect("load reloaded vortex mod via core shadow copy");

    app.update();

    let vel_vortex = *app.world().get::<Velocity>(e).unwrap();
    assert!(vel_vortex.x < -0.1);
    assert!(vel_vortex.y.abs() > 0.05);

    app.unload_mod(mod_id_2)
        .expect("unload reloaded vortex mod");
    let _ = fs::remove_file(&dev_mod_path);
    let _ = fs::remove_dir_all(&staging_dir);
}

#[test]
fn test_visual_game_gravity_physical_kinematics_oracle() {
    let grav_dylib = fixture_dylib("mod_gravity");
    let mut app = setup_visual_game_app();

    let e = app
        .world_mut()
        .spawn((
            Position {
                x: 50.0,
                y: 15.0,
                z: 0.0,
            },
            Velocity {
                x: 10.0,
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

    let mod_id = app
        .load_mod_with(&grav_dylib, shadow_opts())
        .expect("load gravity mod");

    app.update();
    let vel1 = *app.world().get::<Velocity>(e).unwrap();
    let expected_vy = (-40.0 * DT) * 0.999;
    assert!((vel1.y - expected_vy).abs() < 1e-2);

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
    assert!(bounced);

    app.unload_mod(mod_id).expect("unload gravity mod");
}

#[test]
fn test_visual_game_rainbow_color_spectrum_oracle() {
    let rain_dylib = fixture_dylib("mod_rainbow");
    let mut app = setup_visual_game_app();

    let e1 = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 10.0,
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

    let e2 = app
        .world_mut()
        .spawn((
            Position {
                x: 100.0,
                y: 50.0,
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

    let mod_id = app
        .load_mod_with(&rain_dylib, shadow_opts())
        .expect("load rainbow mod");
    app.update();

    let c1 = *app.world().get::<ColorRgba>(e1).unwrap();
    let c2 = *app.world().get::<ColorRgba>(e2).unwrap();

    assert!(c1.r >= 0.0 && c1.r <= 1.0 && c1.g >= 0.0 && c1.g <= 1.0 && c1.b >= 0.0 && c1.b <= 1.0);
    assert!(c2.r >= 0.0 && c2.r <= 1.0 && c2.g >= 0.0 && c2.g <= 1.0 && c2.b >= 0.0 && c2.b <= 1.0);

    let color_delta = (c1.r - c2.r).abs() + (c1.g - c2.g).abs() + (c1.b - c2.b).abs();
    assert!(color_delta > 0.1);

    for _ in 0..30 {
        app.update();
    }
    let c1_later = *app.world().get::<ColorRgba>(e1).unwrap();
    let time_delta =
        (c1.r - c1_later.r).abs() + (c1.g - c1_later.g).abs() + (c1.b - c1_later.b).abs();
    assert!(time_delta > 0.1);

    app.unload_mod(mod_id).expect("unload rainbow mod");
}

#[test]
fn test_visual_game_vortex_orbital_cyclone_oracle() {
    let vort_dylib = fixture_dylib("mod_vortex");
    let mut app = setup_visual_game_app();

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

    let mod_id = app
        .load_mod_with(&vort_dylib, shadow_opts())
        .expect("load vortex mod");
    app.update();

    let vel_orbit = *app.world().get::<Velocity>(e_orbit).unwrap();
    let vel_core = *app.world().get::<Velocity>(e_core).unwrap();

    assert!(vel_orbit.x < -0.05);
    assert!(vel_orbit.y.abs() > 0.01);
    assert!(vel_core.x > 0.05);

    app.unload_mod(mod_id).expect("unload vortex mod");
}

#[test]
fn test_visual_game_boundary_empty_world() {
    let grav_dylib = fixture_dylib("mod_gravity");
    let mut app = setup_visual_game_app();

    let mod_id = app
        .load_mod_with(&grav_dylib, shadow_opts())
        .expect("load into empty world");
    for _ in 0..10 {
        app.update();
    }
    app.unload_mod(mod_id).expect("unload from empty world");
}

#[test]
fn test_visual_game_scale_2500_entities() {
    let rain_dylib = fixture_dylib("mod_rainbow");
    let mut app = setup_visual_game_app();

    for i in 0..2500 {
        let x = 10.0 + (i % 100) as f32;
        let y = 10.0 + (i / 100) as f32 * 1.8;
        app.world_mut().spawn((
            Position { x, y, z: 0.0 },
            Velocity {
                x: 0.5,
                y: 0.5,
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

    let mod_id = app
        .load_mod_with(&rain_dylib, shadow_opts())
        .expect("load rainbow at scale");
    for _ in 0..5 {
        app.update();
    }

    let mut query = app.world_mut().query::<&ColorRgba>();
    assert_eq!(query.iter(app.world()).count(), 2500);

    for col in query.iter(app.world()) {
        assert!(col.r >= 0.0 && col.r <= 1.0);
        assert!(col.g >= 0.0 && col.g <= 1.0);
        assert!(col.b >= 0.0 && col.b <= 1.0);
    }

    app.unload_mod(mod_id).expect("unload rainbow at scale");
}

#[test]
fn test_visual_game_double_unload_and_invalid_mod_id() {
    let grav_dylib = fixture_dylib("mod_gravity");
    let mut app = setup_visual_game_app();

    let mod_id = app
        .load_mod_with(&grav_dylib, shadow_opts())
        .expect("load gravity");
    app.update();

    assert!(app.unload_mod(mod_id).is_ok());
    assert!(app.unload_mod(mod_id).is_err());
    assert!(app.unload_mod(ModId(999_999)).is_err());

    app.update();
}

#[test]
fn test_visual_game_entity_churn_spawn_despawn_with_active_mod() {
    let rain_dylib = fixture_dylib("mod_rainbow");
    let mut app = setup_visual_game_app();

    let mut entities: Vec<Entity> = (0..50)
        .map(|i| {
            app.world_mut()
                .spawn((
                    Position {
                        x: 10.0 + i as f32,
                        y: 15.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 0.5,
                        z: 0.0,
                    },
                    ColorRgba {
                        r: 1.0,
                        g: 1.0,
                        b: 1.0,
                        a: 1.0,
                    },
                ))
                .id()
        })
        .collect();

    let mod_id = app
        .load_mod_with(&rain_dylib, shadow_opts())
        .expect("load rainbow mod for churn");
    app.update();

    for id in entities.drain(0..25) {
        app.world_mut().despawn(id);
    }
    app.update();

    for i in 0..40 {
        let id = app
            .world_mut()
            .spawn((
                Position {
                    x: 50.0 + i as f32,
                    y: 25.0,
                    z: 0.0,
                },
                Velocity {
                    x: -0.5,
                    y: -0.5,
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
    app.update();

    let mut query = app.world_mut().query::<(&Position, &ColorRgba)>();
    assert_eq!(query.iter(app.world()).count(), 65);

    for (pos, col) in query.iter(app.world()) {
        assert!(!pos.x.is_nan() && !pos.y.is_nan());
        assert!(
            col.r >= 0.0
                && col.r <= 1.0
                && col.g >= 0.0
                && col.g <= 1.0
                && col.b >= 0.0
                && col.b <= 1.0
        );
    }

    app.unload_mod(mod_id).expect("unload rainbow after churn");
}

#[test]
fn test_visual_game_color_restoration_oracle() {
    let rain_dylib = fixture_dylib("mod_rainbow");
    let mut app = setup_visual_game_app();

    for i in 0..20 {
        app.world_mut().spawn((
            Position {
                x: 5.0 + i as f32 * 5.0,
                y: 20.0,
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
        ));
    }

    let mod_id = app
        .load_mod_with(&rain_dylib, shadow_opts())
        .expect("load rainbow mod");
    for _ in 0..10 {
        app.update();
    }

    let mut mutated = false;
    for col in app.world_mut().query::<&ColorRgba>().iter(app.world()) {
        if col.r != 1.0 || col.g != 1.0 || col.b != 1.0 {
            mutated = true;
            break;
        }
    }
    assert!(mutated);

    app.unload_mod(mod_id).expect("unload rainbow mod");
    {
        let mut query = app.world_mut().query::<&mut ColorRgba>();
        let mut toggle = false;
        for mut col in query.iter_mut(app.world_mut()) {
            if toggle {
                *col = ColorRgba {
                    r: 0.2,
                    g: 0.8,
                    b: 1.0,
                    a: 1.0,
                };
            } else {
                *col = ColorRgba {
                    r: 1.0,
                    g: 1.0,
                    b: 1.0,
                    a: 1.0,
                };
            }
            toggle = !toggle;
        }
    }

    for _ in 0..5 {
        app.update();
    }

    for col in app.world_mut().query::<&ColorRgba>().iter(app.world()) {
        let is_cyan =
            (col.r - 0.2).abs() < 1e-4 && (col.g - 0.8).abs() < 1e-4 && (col.b - 1.0).abs() < 1e-4;
        let is_white =
            (col.r - 1.0).abs() < 1e-4 && (col.g - 1.0).abs() < 1e-4 && (col.b - 1.0).abs() < 1e-4;
        assert!(is_cyan || is_white);
    }
}
