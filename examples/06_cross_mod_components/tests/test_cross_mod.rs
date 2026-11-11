//! Integration test for Example 6: Cross-Mod Dynamic Components
//!
//! Verifies:
//! 1. Mod A registers runtime component `EnergyShield` without host compile-time knowledge.
//! 2. Mod A spawns entities with `EnergyShield` via deferred commands.
//! 3. Mod B (which neither registered `EnergyShield` nor spawned any entities) successfully
//!    queries, reads, and mutates `EnergyShield` across Bevy's archetype tables.
//! 4. Unloading both mods leaves entity data frozen without panics.

use bevy_app::App;
use host_runtime::prelude::*;
use std::path::PathBuf;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
struct EnergyShield {
    pub current: f32,
    pub max: f32,
    pub frequency_mhz: u32,
}

fn get_dylib_paths() -> (PathBuf, PathBuf) {
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

    if !mod_a.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(manifest_dir.join("mods/mod_shield_emitter/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to build mod_shield_emitter");
        assert!(status.success());
    }

    if !mod_b.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(manifest_dir.join("mods/mod_overcharge_siphon/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to build mod_overcharge_siphon");
        assert!(status.success());
    }

    (mod_a, mod_b)
}

#[test]
fn test_cross_mod_component_query_and_access() {
    let (mod_a_dylib, mod_b_dylib) = get_dylib_paths();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());

    // Phase 1: Load Mod A (Introducer / Spawner)
    let mod_a_id = app
        .load_mod_tracked(&mod_a_dylib)
        .expect("Load mod_shield_emitter");

    let shield_comp_id = {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .get_by_name("EnergyShield")
            .expect("EnergyShield must be registered by Mod A");
        assert!(comp.is_dynamic);
        assert_eq!(comp.size, core::mem::size_of::<EnergyShield>());
        comp.component_id
    };

    // Phase 2: Load Mod B (Consumer / Siphoner)
    // Notice: Mod B did not introduce EnergyShield, but resolves it from ModRegistry!
    let mod_b_id = app
        .load_mod_tracked(&mod_b_dylib)
        .expect("Load mod_overcharge_siphon");

    // Phase 3: Run 5 schedule ticks
    for _ in 1..=5 {
        app.update();
    }

    // Phase 4: Verify entities spawned by Mod A were queried and mutated by Mod B
    let mut drone_shields = Vec::new();
    for entity in app.world().iter_entities() {
        if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
            let shield = unsafe { *(comp_ref.as_ptr() as *const EnergyShield) };
            drone_shields.push(shield);
        }
    }

    assert_eq!(
        drone_shields.len(),
        3,
        "Mod A must have spawned 3 drone entities with EnergyShield"
    );

    // Initial values from Mod A:
    // Drone 1: 100.0, Drone 2: 80.0, Drone 3: 60.0
    // Each tick: Mod A recharges +5.0 (capped at 100), Mod B siphons -12.0.
    // Net per tick: -7.0 MW.
    // Drone 1 (100.0 initial):
    //   Tick 1 (spawn deferred, applies end of frame): 100.0
    //   Tick 2: 100 + 5 (cap 100) - 12 = 88.0
    //   Tick 3: 88 + 5 - 12 = 81.0
    //   Tick 4: 81 + 5 - 12 = 74.0
    //   Tick 5: 74 + 5 - 12 = 67.0
    for s in &drone_shields {
        assert!(
            s.current < s.max,
            "Mod B must have siphoned energy from Mod A's spawned shields: got current={}",
            s.current
        );
        assert!(
            s.frequency_mhz > 140,
            "Frequency must have been modulated by Mod A: got freq={}",
            s.frequency_mhz
        );
    }

    // Phase 5: Unload both mods cleanly
    app.unload_mod(mod_a_id).expect("Unload Mod A");
    app.unload_mod(mod_b_id).expect("Unload Mod B");

    // Capture frozen state
    let frozen_shields: Vec<EnergyShield> = app
        .world()
        .iter_entities()
        .filter_map(|e| e.get_by_id(shield_comp_id).ok())
        .map(|ptr| unsafe { *(ptr.as_ptr() as *const EnergyShield) })
        .collect();

    // Tick post-unload
    app.update();

    let post_unload_shields: Vec<EnergyShield> = app
        .world()
        .iter_entities()
        .filter_map(|e| e.get_by_id(shield_comp_id).ok())
        .map(|ptr| unsafe { *(ptr.as_ptr() as *const EnergyShield) })
        .collect();

    assert_eq!(
        frozen_shields, post_unload_shields,
        "Post-unload tick must not mutate component data"
    );
}
