#![forbid(unsafe_code)]
//! Integration test for Example 08 (`zig_mod/src/root.zig`).
//!
//! Runs full end-to-end Zig guest loading, chunk iteration, error-union mapping, and unload when
//! `zig` is installed (e.g., CI via `goto-bus-stop/setup-zig`), and verifies host registration
//! cleanly when `zig` is absent on a local machine.

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use bevy_app::App;
use bevy_ecs::message::Messages;
use example_08_zig_guest::{Position, Velocity, ZigSimConfig, try_resolve_or_build_zig_mod};

#[test]
fn zig_guest_lifecycle_or_graceful_skip_when_zig_absent() {
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
    app.init_resource::<ZigSimConfig>();
    app.register_mod_resource::<ZigSimConfig>()
        .expect("register ZigSimConfig");

    let entity = app
        .world_mut()
        .spawn((Position::new(2.0, 4.0, 6.0), Velocity::new(8.0, 12.0, -4.0)))
        .id();

    let Some(dylib_path) = try_resolve_or_build_zig_mod() else {
        eprintln!("Skipping Zig runtime load test because `zig` is not installed on PATH.");
        return;
    };

    let mod_id = app
        .load_mod_with(
            &dylib_path,
            LoadOptions {
                shadow_copy: true,
                strict: true,
                ..Default::default()
            },
        )
        .expect("load Zig guest mod");

    app.update();
    assert_eq!(
        *app.world().get::<Position>(entity).unwrap(),
        Position::new(4.0, 7.0, 5.0)
    );

    app.world_mut().resource_mut::<ZigSimConfig>().trigger_error = FfiBool::TRUE;
    app.update();

    let faults = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(faults.len(), 1);
    let mut cursor = faults.get_cursor();
    let fault = cursor.read(faults).next().expect("ModSystemFaulted");
    assert_eq!(fault.status, FfiResult::INVALID_ARGUMENT);

    app.unload_mod(mod_id).expect("unload Zig guest mod");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
}
