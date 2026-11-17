//! Example 1: Minimal ABI `0.2.0` Handshake (Host Application).
//!
//! Demonstrates the minimal lifecycle handshake between a Bevy `World` and a native `.cdylib`
//! guest mod: registering a `#[derive(ModComponent)]` type, shadow-copy loading with
//! [`LoadOptions`], observing [`ModLoaded`] / [`ModUnloaded`] messages, executing guest ticks,
//! and deterministically unloading the mod.

use bevy_abi_mod::prelude::*;
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::message::{MessageReader, Messages};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Shared 3D position component exposed over the C-ABI (`"example::Position"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

impl Position {
    /// Constructs a new [`Position`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

fn log_lifecycle_messages(
    mut loaded: MessageReader<ModLoaded>,
    mut unloaded: MessageReader<ModUnloaded>,
    mut faulted: MessageReader<ModSystemFaulted>,
) {
    for msg in loaded.read() {
        println!(
            "  [Message<ModLoaded>]   id={:?} name={} v{} systems={}",
            msg.id, msg.name, msg.version, msg.system_count
        );
    }
    for msg in unloaded.read() {
        println!(
            "  [Message<ModUnloaded>] id={:?} name={} removed_systems={}",
            msg.id, msg.name, msg.removed_systems
        );
    }
    for msg in faulted.read() {
        eprintln!(
            "  [Message<ModSystemFaulted>] system={} status={:?}",
            msg.system_name, msg.status
        );
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
        .expect("guest manifest parent directory")
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
    println!("=== Example 1: Minimal ABI 0.2.0 Handshake ===");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("Failed to register example::Position");
    app.add_systems(Update, log_lifecycle_messages);

    app.world_mut().spawn(Position::new(0.0, 0.0, 0.0));
    app.world_mut().spawn(Position::new(10.0, 20.0, 30.0));
    app.world_mut().spawn(Position::new(100.0, 200.0, 300.0));

    let guest_cargo = Path::new(env!("CARGO_MANIFEST_DIR")).join("guest_mod/Cargo.toml");
    let dylib_path = ensure_guest_mod_built(&guest_cargo);

    let mod_id = app
        .load_mod_with(
            &dylib_path,
            LoadOptions {
                shadow_copy: true,
                strict: true,
                ..Default::default()
            },
        )
        .expect("Failed to load minimal_guest_mod");
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 1);
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
    app.update();
    let post_unload_positions: Vec<Position> = query.iter(app.world()).copied().collect();
    assert_eq!(positions, post_unload_positions);
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0
    );
    println!("\n[HOST] Unloaded mod cleanly; post-unload tick left state unchanged.");
}
