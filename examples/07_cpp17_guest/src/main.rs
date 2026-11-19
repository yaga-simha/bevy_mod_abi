#![forbid(unsafe_code)]
//! Example 07: Minimal ISO C++17 Guest Mod (Bevy Host Application).
//!
//! Loads `cpp_mod/mod.cpp` (`ABI 0.3.0`), executes damped kinematics over contiguous SoA chunks,
//! demonstrates C++ exception containment (`try / catch (...) -> BAM_RESULT_PANIC_CAUGHT`), and
//! verifies per-`World` RAII `CppModInstance` cleanup on unload.

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use bevy_app::App;
use bevy_ecs::message::Messages;
use example_07_cpp17_guest::{Position, SimConfig, Velocity, resolve_or_build_cpp17_mod};

fn main() {
    println!("=== Example 07: Minimal ISO C++17 Guest Mod (ABI 0.3.0) ===");

    let dylib_path = resolve_or_build_cpp17_mod();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register example::Position");
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register example::Velocity");
    app.init_resource::<SimConfig>();
    app.register_mod_resource::<SimConfig>()
        .expect("register example::SimConfig");

    let e0 = app
        .world_mut()
        .spawn((Position::new(0.0, 0.0, 0.0), Velocity::new(16.0, 8.0, 4.0)))
        .id();
    let e1 = app
        .world_mut()
        .spawn((
            Position::new(10.0, 20.0, 30.0),
            Velocity::new(-8.0, 16.0, 0.0),
        ))
        .id();

    let mod_id = app
        .load_mod_with(
            &dylib_path,
            LoadOptions {
                shadow_copy: true,
                strict: true,
                ..Default::default()
            },
        )
        .expect("load C++17 guest mod");
    println!(
        "[HOST] Loaded C++17 mod {:?} from {}",
        mod_id,
        dylib_path.display()
    );

    // Tick 1: vel = (8, 4, 2), pos = (4, 2, 1)
    // Tick 2: vel = (4, 2, 1), pos = (6, 3, 1.5)
    app.update();
    app.update();

    let p0 = *app.world().get::<Position>(e0).expect("e0 Position");
    let p1 = *app.world().get::<Position>(e1).expect("e1 Position");
    println!("  After 2 ticks -> Entity 0: {p0:?}, Entity 1: {p1:?}");
    assert_eq!(p0, Position::new(6.0, 3.0, 1.5));
    assert_eq!(p1, Position::new(7.0, 26.0, 30.0));

    // Trigger a C++17 exception caught inside `mod.cpp` (`try { ... } catch (...)`)
    app.world_mut()
        .resource_mut::<SimConfig>()
        .trigger_exception = FfiBool::TRUE;
    app.update();

    let faults = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(faults.len(), 1);
    let mut cursor = faults.get_cursor();
    let fault = cursor.read(faults).next().expect("fault message");
    assert_eq!(fault.status, FfiResult::PANIC_CAUGHT);
    println!(
        "  [Fault Contained] system='{}' status={} (quarantined cleanly)",
        fault.system_name, fault.status
    );

    app.unload_mod(mod_id).expect("unload C++17 guest mod");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
    println!("[HOST] Unloaded C++17 guest mod cleanly (RAII ~CppModInstance ran).");
}
