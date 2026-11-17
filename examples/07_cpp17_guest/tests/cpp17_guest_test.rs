#![forbid(unsafe_code)]
//! End-to-end integration tests for Example 07 (`cpp_mod/mod.cpp`, ISO C++17).

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use bevy_app::App;
use bevy_ecs::message::Messages;
use example_07_cpp17_guest::{Position, SimConfig, Velocity, resolve_or_build_cpp17_mod};

fn build_configured_app() -> App {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Position");
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register Velocity");
    app.init_resource::<SimConfig>();
    app.register_mod_resource::<SimConfig>()
        .expect("register SimConfig");
    app
}

#[test]
fn cpp17_guest_chunk_iteration_multi_world_raii_and_exception_containment() {
    let dylib_path = resolve_or_build_cpp17_mod();
    let opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };

    // Load the same C++17 shared library simultaneously into two independent Bevy Worlds
    // to verify zero process-global state and independent RAII `CppModInstance` lifecycles.
    let mut app_a = build_configured_app();
    let mut app_b = build_configured_app();
    app_b.world_mut().resource_mut::<SimConfig>().dt = 1.0;

    let entity_a = app_a
        .world_mut()
        .spawn((Position::new(0.0, 0.0, 0.0), Velocity::new(16.0, 8.0, 4.0)))
        .id();
    let entity_b = app_b
        .world_mut()
        .spawn((
            Position::new(1.0, 2.0, 3.0),
            Velocity::new(10.0, 20.0, 30.0),
        ))
        .id();

    let mod_a = app_a
        .load_mod_with(&dylib_path, opts.clone())
        .expect("load C++17 mod into World A");
    let mod_b = app_b
        .load_mod_with(&dylib_path, opts)
        .expect("load C++17 mod into World B");

    app_a.update();
    app_b.update();

    assert_eq!(
        *app_a.world().get::<Position>(entity_a).unwrap(),
        Position::new(4.0, 2.0, 1.0)
    );
    assert_eq!(
        *app_b.world().get::<Position>(entity_b).unwrap(),
        Position::new(6.0, 12.0, 18.0)
    );

    // Unload World A and verify World B continues running unaffected.
    app_a.unload_mod(mod_a).expect("unload World A mod");
    app_b.update();
    assert_eq!(
        *app_b.world().get::<Position>(entity_b).unwrap(),
        Position::new(8.5, 17.0, 25.5)
    );

    // Trigger a C++17 exception inside World B and verify `BAM_RESULT_PANIC_CAUGHT` containment.
    app_b
        .world_mut()
        .resource_mut::<SimConfig>()
        .trigger_exception = FfiBool::TRUE;
    app_b.update();

    let faults = app_b.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(faults.len(), 1);
    let mut cursor = faults.get_cursor();
    let fault = cursor.read(faults).next().expect("ModSystemFaulted");
    assert_eq!(fault.mod_id, Some(mod_b));
    assert_eq!(fault.system_name, "cpp17_damped_kinematics");
    assert_eq!(fault.status, FfiResult::PANIC_CAUGHT);

    // Position in World B remains unchanged on the faulted tick and after quarantine.
    assert_eq!(
        *app_b.world().get::<Position>(entity_b).unwrap(),
        Position::new(8.5, 17.0, 25.5)
    );

    app_b.unload_mod(mod_b).expect("unload World B mod");
    assert_eq!(app_b.world().resource::<Messages<ModUnloaded>>().len(), 1);
}
