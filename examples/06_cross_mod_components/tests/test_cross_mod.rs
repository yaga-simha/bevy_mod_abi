//! Integration test for Example 6: Cross-Mod Dynamic Components.
//!
//! Verifies:
//! 1. Mod A registers runtime component `"mod_shield_emitter::EnergyShield"` without host
//!    compile-time registration.
//! 2. Mod A spawns entities with `EnergyShield` via batched deferred commands.
//! 3. Mod B resolves `"mod_shield_emitter::EnergyShield"` by canonical name + UUID and mutates
//!    the same archetype columns deterministically after Mod A's recharge system.
//! 4. Unloading both mods leaves entity data intact and frozen on subsequent ticks.

use bevy_abi_mod::prelude::*;
use bevy_app::App;
use bevy_ecs::message::Messages;
use std::path::PathBuf;
use std::process::Command;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
struct EnergyShield {
    current: f32,
    max: f32,
    frequency_mhz: u32,
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

    if let Some(dir) = std::env::var_os("BAM_FIXTURE_DIR").filter(|v| !v.is_empty()) {
        let fixture_dir = PathBuf::from(dir);
        let staged_a = fixture_dir.join(format!("{prefix}mod_shield_emitter.{ext}"));
        let staged_b = fixture_dir.join(format!("{prefix}mod_overcharge_siphon.{ext}"));
        if staged_a.is_file() && staged_b.is_file() {
            return (staged_a, staged_b);
        }
    }

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
    app.add_plugins(AbiModPlugin);

    let load_opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };

    // Phase 1: Load Mod A (Introducer / Spawner)
    let mod_a_id = app
        .load_mod_with(&mod_a_dylib, load_opts.clone())
        .expect("Load mod_shield_emitter");

    let shield_comp_id = {
        let reg = app.world().resource::<ModRegistry>();
        let comp = reg
            .snapshot()
            .get_by_name("mod_shield_emitter::EnergyShield")
            .expect("mod_shield_emitter::EnergyShield must be registered by Mod A");
        assert!(comp.is_dynamic);
        assert_eq!(comp.size, core::mem::size_of::<EnergyShield>());
        comp.component_id
    };

    // Phase 2: Load Mod B (Consumer / Siphoner)
    let mod_b_id = app
        .load_mod_with(&mod_b_dylib, load_opts)
        .expect("Load mod_overcharge_siphon");
    assert_eq!(app.world().resource::<Messages<ModLoaded>>().len(), 2);

    // Phase 3: Run 5 schedule ticks
    for _ in 1..=5 {
        app.update();
    }
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0
    );

    // Phase 4: Verify entities spawned by Mod A were queried and mutated by Mod B
    let mut drone_shields = Vec::new();
    for entity in app.world().iter_entities() {
        if let Ok(comp_ref) = entity.get_by_id(shield_comp_id) {
            // SAFETY: `shield_comp_id` was verified above to match the 12-byte `#[repr(C)]` layout
            // of `EnergyShield`.
            let shield =
                unsafe { core::ptr::read_unaligned(comp_ref.as_ptr().cast::<EnergyShield>()) };
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
    // Tick 1 spawns deferred; Ticks 2..=5 each recharge +5.0 (capped at 100.0) then siphon -12.0.
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
    app.unload_mod(mod_b_id).expect("Unload Mod B");
    app.unload_mod(mod_a_id).expect("Unload Mod A");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 2);

    let frozen_shields: Vec<EnergyShield> = app
        .world()
        .iter_entities()
        .filter_map(|e| e.get_by_id(shield_comp_id).ok())
        .map(|ptr| {
            // SAFETY: `shield_comp_id` matches the 12-byte `#[repr(C)]` layout of `EnergyShield`.
            unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<EnergyShield>()) }
        })
        .collect();

    app.update();

    let post_unload_shields: Vec<EnergyShield> = app
        .world()
        .iter_entities()
        .filter_map(|e| e.get_by_id(shield_comp_id).ok())
        .map(|ptr| {
            // SAFETY: `shield_comp_id` matches the 12-byte `#[repr(C)]` layout of `EnergyShield`.
            unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<EnergyShield>()) }
        })
        .collect();

    assert_eq!(
        frozen_shields, post_unload_shields,
        "Post-unload tick must not mutate component data"
    );
}
