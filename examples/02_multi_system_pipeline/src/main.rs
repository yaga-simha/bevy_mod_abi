//! Example 2: Multi-System Pipeline (Host Application)
//!
//! Demonstrates multi-component queries, all 4 query access modes (READ, WRITE,
//! FILTER_ONLY, WITHOUT), deferred commands (spawn_empty, insert_value, despawn),
//! and automatic change detection integration.

#![allow(clippy::default_constructed_unit_structs, clippy::type_complexity)]

mod components;

use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::query::Changed;
use bevy_ecs::system::Query;
use components::*;
use host_runtime::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ensure_guest_mod_built(guest_manifest: &Path) -> PathBuf {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let dylib = guest_manifest
        .parent()
        .unwrap()
        .join("target/release")
        .join(format!("{prefix}example_02_guest_mod.{ext}"));

    if !dylib.exists() {
        println!("[HOST] Compiling guest mod cdylib...");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(guest_manifest)
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for guest mod");
        assert!(status.success(), "Guest mod build failed");
    }
    dylib
}

// Host Observer 1: Motion change detection
fn monitor_motion_changes(
    query: Query<(Entity, &Position, Option<&PlayerTag>, Option<&DroneTag>), Changed<Position>>,
) {
    for (entity, pos, player_tag, drone_tag) in query.iter() {
        let tag = if player_tag.is_some() {
            "[Player]"
        } else if drone_tag.is_some() {
            "[Drone]"
        } else {
            "[Other]"
        };
        println!(
            "  [Changed<Position>] {:<8} {:?} -> ({:.1}, {:.1}, {:.1})",
            tag, entity, pos.x, pos.y, pos.z
        );
    }
}

// Host Observer 2: Health change detection
fn monitor_health_changes(
    query: Query<(Entity, &Health, Option<&PlayerTag>, Option<&DroneTag>), Changed<Health>>,
) {
    for (entity, health, player_tag, drone_tag) in query.iter() {
        let tag = if player_tag.is_some() {
            "[Player]"
        } else if drone_tag.is_some() {
            "[Drone]"
        } else {
            "[Other]"
        };
        println!(
            "  [Changed<Health>]   {:<8} {:?} -> Health: {:.1} / {:.1}",
            tag, entity, health.current, health.max
        );
    }
}

fn main() {
    println!("============================================================");
    println!(" bevy_abi_mod: Example 2 (Multi-System Pipeline)");
    println!(" Demonstrating Queries, Filters, Deferred Commands & Ticks");
    println!("============================================================\n");

    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let guest_cargo = manifest_dir.join("guest_mod/Cargo.toml");
    let guest_dylib = ensure_guest_mod_built(&guest_cargo);

    let mut app = App::new();

    // 1. Initialize modding subsystem
    app.add_plugins(AbiModPlugin::default());

    // 2. Register exposable components
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Health>();
    app.register_mod_component::<PlayerTag>();
    app.register_mod_component::<DroneTag>();

    // 3. Add host change observer systems
    app.add_systems(Update, (monitor_motion_changes, monitor_health_changes));

    // 4. Spawn initial entities
    println!(">>> [Host] Spawning initial simulation entities...");
    let e_player = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 5.0,
                y: 5.0,
                z: 0.0,
            },
            Health {
                current: 80.0,
                max: 100.0,
            },
            PlayerTag { id: 1 },
        ))
        .id();
    println!("  -> Spawned Player Ship: {:?}", e_player);

    let e_alpha = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            Health {
                current: 50.0,
                max: 50.0,
            },
            DroneTag { squad: 1 },
        ))
        .id();
    println!("  -> Spawned Minion Drone Alpha: {:?}", e_alpha);

    let e_beta = app
        .world_mut()
        .spawn((
            Position {
                x: 20.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 0.0,
                y: 2.0,
                z: 0.0,
            },
            Health {
                current: 10.0,
                max: 30.0,
            },
            DroneTag { squad: 1 },
        ))
        .id();
    println!("  -> Spawned Minion Drone Beta (low HP): {:?}", e_beta);

    let e_beacon = app
        .world_mut()
        .spawn((
            Position {
                x: 100.0,
                y: 100.0,
                z: 0.0,
            },
            Health {
                current: 50.0,
                max: 50.0,
            },
        ))
        .id();
    println!("  -> Spawned Stationary Relay Beacon: {:?}", e_beacon);

    // Initial tick to flush entity spawns
    app.update();

    // 5. Load guest mod library
    println!("\n>>> [Host] Loading guest mod: {:?}", guest_dylib);
    let mod_id = app
        .load_mod_tracked(&guest_dylib)
        .expect("Failed to load guest mod");
    println!(
        ">>> [Host] Successfully loaded mod with ModId: {:?}\n",
        mod_id
    );

    // 6. Execute 5 simulation ticks
    for tick in 1..=5 {
        println!("------------------------------------------------------------");
        println!(">>> Simulation Tick {}", tick);
        println!("------------------------------------------------------------");
        app.update();
    }

    // 7. Verify simulation assertions
    println!("\n>>> [Host] Verifying post-simulation invariants...");

    // Invariant A: Player health increased (PlayerRegenSystem boosted player via FILTER_ONLY PlayerTag)
    let player_health = app.world().get::<Health>(e_player).unwrap();
    assert!(
        player_health.current > 80.0,
        "Player must have been healed by PlayerRegenSystem"
    );
    println!(
        "  [OK] Invariant A: Player health boosted to {:.1} / {:.1}",
        player_health.current, player_health.max
    );

    // Invariant B: Minion Drone Beta was despawned by FleetLifecycleSystem when health hit 0
    assert!(
        app.world().get_entity(e_beta).is_err(),
        "Drone Beta must be despawned after reaching 0 HP"
    );
    println!("  [OK] Invariant B: Minion Drone Beta ({:?}) successfully despawned via CommandQueueBridge", e_beta);

    // Invariant C: Reinforcement Drone Gamma was spawned deferred and has Position and DroneTag
    let mut drone_gamma_found = false;
    let mut query = app.world_mut().query::<(
        Entity,
        &Position,
        &Health,
        Option<&DroneTag>,
        Option<&PlayerTag>,
    )>();
    for (entity, pos, health, drone_tag, player_tag) in query.iter(app.world()) {
        if player_tag.is_none() && drone_tag.is_some() && entity != e_alpha && entity != e_beacon {
            drone_gamma_found = true;
            println!("  [OK] Invariant C: Detected reinforcement drone ({:?}) at ({:.1}, {:.1}) with HP {:.1}", entity, pos.x, pos.y, health.current);
        }
    }
    assert!(
        drone_gamma_found,
        "Reinforcement drone must have been spawned deferred by mod"
    );

    // 8. Cleanly unload mod
    println!("\n>>> [Host] Unloading mod {:?}", mod_id);
    app.unload_mod(mod_id).expect("Failed to unload mod");
    println!(">>> [Host] Mod unloaded successfully. All assertions passed!\n");
}
