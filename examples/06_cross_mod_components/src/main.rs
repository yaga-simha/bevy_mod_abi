//! # Example 6: Cross-Mod Dynamic Components
//!
//! Demonstrates:
//! 1. One loaded mod (`mod_shield_emitter`) introducing brand-new mod-able components (`EnergyShield`)
//!    at runtime without host compile-time knowledge.
//! 2. That same mod spawning autonomous entities possessing this new component via deferred commands.
//! 3. A second loaded mod (`mod_overcharge_siphon`) querying and accessing/mutating those exact same
//!    components across Bevy's archetype tables, despite NOT having introduced the component or spawned
//!    the entities itself.
//! 4. Hardware-speed, zero-serialization data sharing across dynamic libraries over the Bevy C-ABI boundary.

mod components;

use bevy_app::App;
use components::FleetSector;
use host_runtime::prelude::*;
use std::path::PathBuf;
use std::process::Command;

/// Host-side mirror used to decode and display the runtime component values for console telemetry.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct EnergyShield {
    pub current: f32,
    pub max: f32,
    pub frequency_mhz: u32,
}

/// Returns the file paths for the compiled guest mod dynamic libraries.
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

/// Ensures both guest mod dynamic libraries are compiled before loading.
pub fn ensure_mods_built() -> (PathBuf, PathBuf) {
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
    println!("  Mod A introduces components; Mod B queries & accesses them across the C-ABI  ");
    println!("================================================================================\n");

    // 1. Ensure both guest mods are built
    let (mod_a_dylib, mod_b_dylib) = ensure_mods_built();

    // 2. Initialize Bevy App with AbiModPlugin
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<FleetSector>();

    // Spawn a base host entity (e.g. flagship headquarters)
    let flagship = app
        .world_mut()
        .spawn(FleetSector {
            sector_id: 7,
            danger_level: 0.85,
        })
        .id();
    println!("[HOST] Phase 1: Engine Initialization");
    println!(
        "  • Host registered base component `FleetSector` and spawned Flagship {:?}",
        flagship
    );
    println!("  • NOTE: Host has ZERO compile-time registration for `EnergyShield`!\n");

    // 3. Load Mod A (The Introducer / Spawner)
    println!("[HOST] Phase 2: Loading Mod A (`mod_shield_emitter`)");
    let mod_a_id = app
        .load_mod_tracked(&mod_a_dylib)
        .expect("Failed to load mod_shield_emitter");
    println!("  • Successfully loaded Mod A (ID: {:?})", mod_a_id);

    // Verify Mod A registered the component in ModRegistry
    let shield_comp_id = {
        let reg = app.world().resource::<ModRegistry>();
        let comp_meta = reg
            .get_by_name("EnergyShield")
            .expect("Mod A must have registered 'EnergyShield'");
        println!(
            "  • Mod A registered runtime component 'EnergyShield' (ComponentId: {:?}, Size: {}B, Align: {}B)",
            comp_meta.component_id, comp_meta.size, comp_meta.align
        );
        comp_meta.component_id
    };
    println!();

    // 4. Load Mod B (The Consumer / Siphoner)
    println!("[HOST] Phase 3: Loading Mod B (`mod_overcharge_siphon`)");
    println!("  • Notice: Mod B did NOT introduce or register 'EnergyShield'!");
    println!("  • Notice: Mod B did NOT spawn any entities!");
    let mod_b_id = app
        .load_mod_tracked(&mod_b_dylib)
        .expect("Failed to load mod_overcharge_siphon");
    println!("  • Successfully loaded Mod B (ID: {:?})", mod_b_id);
    println!(
        "  • Mod B successfully resolved 'EnergyShield' from ModRegistry and bound its system!\n"
    );

    // 5. Run simulation frames
    println!("[HOST] Phase 4: Simulating 5 Engine Ticks");
    println!("--------------------------------------------------------------------------------");

    for frame in 1..=5 {
        println!(">>> Frame {}:", frame);
        app.update();

        // Host dynamic inspection of all entities possessing `EnergyShield`
        let mut drone_count = 0;
        for entity in app.world().iter_entities() {
            if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
                drone_count += 1;
                // SAFETY: We mirror the C-ABI layout of `EnergyShield` (12 bytes).
                let shield = unsafe { *(comp_ref.as_ptr() as *const EnergyShield) };
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

    // 6. Verify cross-mod interactions mathematically
    println!("[HOST] Phase 5: Verification & Telemetry Summary");
    let mut final_shields = Vec::new();
    for entity in app.world().iter_entities() {
        if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
            let shield = unsafe { *(comp_ref.as_ptr() as *const EnergyShield) };
            final_shields.push(shield);
        }
    }

    println!(
        "  • Total drones managed across both mods: {}",
        final_shields.len()
    );
    for (i, s) in final_shields.iter().enumerate() {
        println!(
            "  • Drone #{}: Final Shield = {:.1} MW (Max: {:.1} MW, Freq: {} MHz)",
            i + 1,
            s.current,
            s.max,
            s.frequency_mhz
        );
        // Verified: values changed from initial because Mod B siphoned from Mod A's spawned shields
        assert!(
            s.current < s.max,
            "Mod B must have siphoned energy from Mod A's shields"
        );
    }

    // 7. Clean unload
    app.unload_mod(mod_a_id).expect("Unload Mod A");
    app.unload_mod(mod_b_id).expect("Unload Mod B");
    println!("\n[HOST] Successfully unloaded both mods cleanly. Example 6 completed!");
}
