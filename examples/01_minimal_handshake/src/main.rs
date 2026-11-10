//! Example 1: Minimal Handshake Proof of Concept (Host Application)
#![allow(clippy::default_constructed_unit_structs)]
use bevy_app::App;
use bevy_ecs::component::Component;
use host_runtime::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Position {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

fn ensure_guest_mod_built(manifest: &Path) -> PathBuf {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let dylib = manifest
        .parent()
        .unwrap()
        .join("target/release")
        .join(format!("{prefix}minimal_guest_mod.{ext}"));

    if !dylib.exists() {
        println!("[HOST] Compiling guest mod cdylib...");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(manifest)
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for guest mod");
        assert!(status.success(), "Guest mod build failed");
    }
    dylib
}

fn main() {
    println!("=== Example 1: Minimal Handshake PoC ===");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();

    app.world_mut().spawn(Position::new(0.0, 0.0, 0.0));
    app.world_mut().spawn(Position::new(10.0, 20.0, 30.0));
    app.world_mut().spawn(Position::new(100.0, 200.0, 300.0));

    let guest_cargo = Path::new(env!("CARGO_MANIFEST_DIR")).join("guest_mod/Cargo.toml");
    let dylib_path = ensure_guest_mod_built(&guest_cargo);

    let mod_id = app
        .load_mod_tracked(&dylib_path)
        .expect("Failed to load mod");
    println!(
        "[HOST] Loaded mod {:?} from {}",
        mod_id,
        dylib_path.display()
    );

    println!("\n[HOST] Initial entity positions:");
    let mut query = app.world_mut().query::<&Position>();
    for (i, p) in query.iter(app.world()).enumerate() {
        println!("  Entity {i}: ({:.1}, {:.1}, {:.1})", p.x, p.y, p.z);
    }

    println!("\n[HOST] Running 3 Bevy update ticks...");
    for tick in 1..=3 {
        app.update();
        println!("  Tick {tick} completed.");
    }

    println!("\n[HOST] Entity positions after 3 ticks:");
    let positions: Vec<Position> = query.iter(app.world()).copied().collect();
    for (i, p) in positions.iter().enumerate() {
        println!("  Entity {i}: ({:.1}, {:.1}, {:.1})", p.x, p.y, p.z);
    }

    assert_eq!(positions[0], Position::new(3.0, 6.0, 9.0));
    assert_eq!(positions[1], Position::new(13.0, 26.0, 39.0));
    assert_eq!(positions[2], Position::new(103.0, 206.0, 309.0));

    app.unload_mod(mod_id).expect("Failed to unload mod");
    println!("\n[HOST] Successfully unloaded mod. Handshake verification PASSED!");
}
