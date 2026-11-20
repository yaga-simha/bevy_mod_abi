//! Example 6: Cross-Mod Dynamic Components (Host Application).
//!
//! Demonstrates:
//! 1. Mod A (`mod_shield_emitter`) registering a runtime dynamic component
//!    (`"mod_shield_emitter::EnergyShield"`) without host compile-time registration.
//! 2. Mod A spawning autonomous drone entities carrying `EnergyShield` via batched deferred commands.
//! 3. Mod B (`mod_overcharge_siphon`) resolving `"mod_shield_emitter::EnergyShield"` by canonical
//!    name + 128-bit `ComponentUuid` and mutating those columns in Bevy's schedule.

/// Exposed ECS components for Example 6.
pub mod components;

use bevy_app::App;
use bevy_ecs::message::Messages;
use bevy_mod_abi::prelude::*;
use components::FleetSector;
use std::path::PathBuf;
use std::process::Command;

/// Host-side layout mirror used only to print console telemetry for `"mod_shield_emitter::EnergyShield"`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct EnergyShieldTelemetry {
    current: f32,
    max: f32,
    frequency_mhz: u32,
}

fn get_guest_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let mod_a = manifest_dir
        .join("mods/mod_shield_emitter/target/release")
        .join(format!("{prefix}mod_shield_emitter.{ext}"));
    let mod_b = manifest_dir
        .join("mods/mod_overcharge_siphon/target/release")
        .join(format!("{prefix}mod_overcharge_siphon.{ext}"));

    (mod_a, mod_b)
}

fn ensure_mods_built() -> (PathBuf, PathBuf) {
    let (mod_a, mod_b) = get_guest_dylib_paths();
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    if !mod_a.exists() {
        println!("[HOST] Compiling Mod A (mod_shield_emitter) via cargo...");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(manifest_dir.join("mods/mod_shield_emitter/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to build mod_shield_emitter");
        assert!(status.success(), "Failed to build mod_shield_emitter");
    }

    if !mod_b.exists() {
        println!("[HOST] Compiling Mod B (mod_overcharge_siphon) via cargo...");
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(manifest_dir.join("mods/mod_overcharge_siphon/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to build mod_overcharge_siphon");
        assert!(status.success(), "Failed to build mod_overcharge_siphon");
    }

    (mod_a, mod_b)
}

fn main() {
    println!("================================================================================");
    println!("  bevy_abi_mod: Example 6 — Cross-Mod Dynamic Components                       ");
    println!("  Mod A introduces components; Mod B queries & mutates them across the C-ABI   ");
    println!("================================================================================\n");

    let (mod_a_dylib, mod_b_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<FleetSector>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register FleetSector");

    let flagship = app
        .world_mut()
        .spawn(FleetSector {
            sector_id: 7,
            danger_level: 0.85,
        })
        .id();
    println!("[HOST] Step 1: Engine Initialization");
    println!("  - Registered `example::FleetSector` and spawned Flagship {flagship:?}");
    println!(
        "  - Host has no compile-time component registration for `mod_shield_emitter::EnergyShield`\n"
    );

    let load_opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };

    println!("[HOST] Step 2: Loading Mod A (`mod_shield_emitter`)");
    let mod_a_id = app
        .load_mod_with(&mod_a_dylib, load_opts.clone())
        .expect("Failed to load mod_shield_emitter");
    println!("  - Loaded Mod A (ID: {mod_a_id:?})");

    let shield_comp_id = {
        let reg = app.world().resource::<ModRegistry>();
        let comp_meta = reg
            .snapshot()
            .get_by_name("mod_shield_emitter::EnergyShield")
            .expect("Mod A must have registered 'mod_shield_emitter::EnergyShield'");
        assert_eq!(
            comp_meta.size,
            core::mem::size_of::<EnergyShieldTelemetry>()
        );
        println!(
            "  - Mod A registered runtime component '{}' (Handle: {:?}, ComponentId: {:?}, Size: {}B, Align: {}B)\n",
            comp_meta.name,
            comp_meta.handle,
            comp_meta.component_id,
            comp_meta.size,
            comp_meta.align
        );
        comp_meta.component_id
    };

    println!("[HOST] Step 3: Loading Mod B (`mod_overcharge_siphon`)");
    let mod_b_id = app
        .load_mod_with(&mod_b_dylib, load_opts)
        .expect("Failed to load mod_overcharge_siphon");
    println!(
        "  - Loaded Mod B (ID: {mod_b_id:?}) and resolved `mod_shield_emitter::EnergyShield`\n"
    );
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 2);

    println!("[HOST] Step 4: Simulating 5 Engine Ticks");
    println!("--------------------------------------------------------------------------------");

    for frame in 1..=5 {
        println!(">>> Frame {frame}:");
        app.update();

        let mut drone_count = 0;
        for entity in app.world().iter_entities() {
            if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
                drone_count += 1;
                // SAFETY: `shield_comp_id` was verified above to have the exact 12-byte `#[repr(C)]`
                // layout of `EnergyShieldTelemetry` (`{ f32, f32, u32 }`), and `read_unaligned` avoids
                // alignment assumptions on the untyped `Ptr`.
                let shield = unsafe {
                    core::ptr::read_unaligned(comp_ref.as_ptr().cast::<EnergyShieldTelemetry>())
                };
                println!(
                    "    [Host Telemetry] Drone {:?} => EnergyShield: {:.1} / {:.1} MW | Freq: {} MHz",
                    entity.id(),
                    shield.current,
                    shield.max,
                    shield.frequency_mhz
                );
            }
        }
        assert!(
            drone_count >= 3,
            "Must have at least 3 drones spawned by Mod A"
        );
        println!();
    }

    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0
    );

    println!("[HOST] Step 5: Verification & Telemetry Summary");
    let mut final_shields = Vec::new();
    for entity in app.world().iter_entities() {
        if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
            // SAFETY: Verified 12-byte `#[repr(C)]` layout for `shield_comp_id`.
            let shield = unsafe {
                core::ptr::read_unaligned(comp_ref.as_ptr().cast::<EnergyShieldTelemetry>())
            };
            final_shields.push(shield);
        }
    }

    println!(
        "  - Total drones managed across both mods: {}",
        final_shields.len()
    );
    for (i, s) in final_shields.iter().enumerate() {
        println!(
            "  - Drone #{}: Final Shield = {:.1} MW (Max: {:.1} MW, Freq: {} MHz)",
            i + 1,
            s.current,
            s.max,
            s.frequency_mhz
        );
        assert!(
            s.current < s.max,
            "Mod B must have siphoned energy from Mod A's shields"
        );
    }

    app.unload_mod(mod_b_id).expect("Unload Mod B");
    app.unload_mod(mod_a_id).expect("Unload Mod A");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 2);
    println!("\n[HOST] Unloaded both mods cleanly. Example 6 completed.");
}
