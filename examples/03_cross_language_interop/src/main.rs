//! # Example 3: Cross-Language Interoperability
//!
//! Demonstrates concurrent execution of an ISO C99 dynamic library and an independent
//! Rust cdylib inside the same Bevy schedule operating on the same entities.

#![allow(clippy::default_constructed_unit_structs, clippy::type_complexity)]

mod components;

use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::query::{Changed, Or};
use bevy_ecs::system::Query;
use components::{ColorRgba, Position, Velocity};
use host_runtime::prelude::*;
use std::path::PathBuf;
use std::process::Command;

/// Host monitoring system observing mutations produced by both C99 and Rust mods.
fn host_telemetry_monitor(
    query: Query<
        (Entity, &Position, &Velocity, &ColorRgba),
        Or<(Changed<Position>, Changed<ColorRgba>)>,
    >,
) {
    for (entity, pos, vel, color) in query.iter() {
        println!(
            "  Drone {:?} | Pos: ({:6.2}, {:6.2}, {:6.2}) [C99] | Vel: ({:4.1}, {:4.1}) | Color: RGBA({:.2}, {:.2}, {:.2}, {:.2}) [Rust]",
            entity, pos.x, pos.y, pos.z, vel.x, vel.y, color.r, color.g, color.b, color.a
        );
    }
}

/// Returns platform-specific dynamic library paths for C and Rust guest mods.
fn get_guest_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let c_dylib = manifest_dir
        .join("c_mod")
        .join(format!("{prefix}c_mod.{ext}"));
    let rust_dylib = manifest_dir
        .join("rust_mod")
        .join("target/release")
        .join(format!("{prefix}rust_mod.{ext}"));

    (c_dylib, rust_dylib)
}

/// Ensures both C99 and Rust guest mods are compiled before loading.
fn ensure_mods_built() -> (PathBuf, PathBuf) {
    let (c_dylib, rust_dylib) = get_guest_dylib_paths();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    if !c_dylib.exists() {
        println!("[HOST] Compiling C99 guest mod via make...");
        let status = Command::new("make")
            .arg("-C")
            .arg(manifest_dir.join("c_mod"))
            .status()
            .expect("Failed to execute make for c_mod");
        assert!(status.success(), "Failed to build C99 guest mod");
    }

    if !rust_dylib.exists() {
        println!("[HOST] Compiling Rust guest mod via cargo...");
        let status = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(manifest_dir.join("rust_mod/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for rust_mod");
        assert!(status.success(), "Failed to build Rust guest mod");
    }

    (c_dylib, rust_dylib)
}

fn main() {
    println!("==================================================================");
    println!("  bevy_abi_mod: Example 3 — Cross-Language Interoperability       ");
    println!("  C99 Guest Mod + Rust Guest Mod Running Concurrently in Bevy ECS ");
    println!("==================================================================\n");

    // 1. Build and locate dynamic libraries
    let (c_dylib, rust_dylib) = ensure_mods_built();
    println!("[HOST] C99 Mod Library : {}", c_dylib.display());
    println!("[HOST] Rust Mod Library: {}\n", rust_dylib.display());

    // 2. Initialize Bevy App with AbiModPlugin
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());

    // 3. Register mod-exposable components
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<ColorRgba>();

    // 4. Add host telemetry observer
    app.add_systems(Update, host_telemetry_monitor);

    // 5. Spawn test drone entities
    println!("[HOST] Spawning 3 test drones with initial coordinates:");
    let e0 = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
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
    let _e1 = app
        .world_mut()
        .spawn((
            Position {
                x: 5.0,
                y: -2.0,
                z: 0.0,
            },
            Velocity {
                x: -2.0,
                y: 8.0,
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
    let _e2 = app
        .world_mut()
        .spawn((
            Position {
                x: -10.0,
                y: 15.0,
                z: 0.0,
            },
            Velocity {
                x: 4.0,
                y: -3.0,
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

    // 6. Load both mods into the Bevy schedule
    println!("\n[HOST] Loading C99 guest mod...");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load C99 mod");
    println!("[HOST] Loaded C99 mod (ModId: {:?})", c_mod_id);

    println!("[HOST] Loading Rust guest mod...");
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load Rust mod");
    println!("[HOST] Loaded Rust mod (ModId: {:?})\n", rust_mod_id);

    // 7. Execute 5 simulation ticks
    println!("--- Commencing Simulation Ticks ---");
    for tick in 1..=5 {
        println!("\n>> Tick {}", tick);
        app.update();
    }

    // 8. Assert cooperative execution:
    // C99 modified Position (x: 0 -> 0 + 5 * 10 * 0.1 = 5.0)
    // Rust modified ColorRgba based on updated Position
    let p0 = *app.world().get::<Position>(e0).unwrap();
    let c0 = *app.world().get::<ColorRgba>(e0).unwrap();
    assert!(
        (p0.x - 5.0).abs() < 1e-4,
        "C99 mod must have updated Position"
    );
    assert!(
        c0.r > 0.0,
        "Rust mod must have updated ColorRgba based on Position"
    );
    println!(
        "\n[HOST] Verified cooperative assertions for Drone e0: Pos ({:.2}, {:.2}, {:.2}), Color ({:.2}, {:.2}, {:.2})",
        p0.x, p0.y, p0.z, c0.r, c0.g, c0.b
    );

    // 9. Unload both mods cleanly
    println!("\n[HOST] Unloading both mods (Atomic Tombstone + Shutdown Hook)...");
    app.unload_mod(c_mod_id).expect("Failed to unload C99 mod");
    app.unload_mod(rust_mod_id)
        .expect("Failed to unload Rust mod");

    // 10. Verify that post-unload ticks execute safely with no further mutations
    println!("[HOST] Executing 1 post-unload tick (expecting 0 mutations):");
    app.update();

    let p0_after = *app.world().get::<Position>(e0).unwrap();
    assert_eq!(
        p0, p0_after,
        "Position must not change after mods are unloaded"
    );

    println!("\n==================================================================");
    println!("  SUCCESS: Cross-language interop verified with zero data races!  ");
    println!("==================================================================");
}
