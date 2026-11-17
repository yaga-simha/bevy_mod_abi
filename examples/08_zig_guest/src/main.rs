#![forbid(unsafe_code)]
//! Example 08: Minimal Zig Guest Mod (Bevy Host Application).
//!
//! Loads `zig_mod/src/root.zig` (`ABI 0.2.0`), executes zero-copy chunk integration, verifies Zig
//! error-union mapping (`error.InvalidArgument -> FfiResult::INVALID_ARGUMENT`), and gracefully
//! skips runtime execution when `zig` is not installed locally.

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::{FfiBool, FfiResult};
use bevy_app::App;
use bevy_ecs::message::Messages;
use example_08_zig_guest::{Position, Velocity, ZigSimConfig, try_resolve_or_build_zig_mod};

fn main() {
    println!("=== Example 08: Minimal Zig Guest Mod (ABI 0.2.0) ===");

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
    app.init_resource::<ZigSimConfig>();
    app.register_mod_resource::<ZigSimConfig>()
        .expect("register example::ZigSimConfig");

    let e0 = app
        .world_mut()
        .spawn((Position::new(1.0, 2.0, 3.0), Velocity::new(8.0, -4.0, 12.0)))
        .id();

    let Some(dylib_path) = try_resolve_or_build_zig_mod() else {
        println!(
            "[SKIP] `zig` toolchain not found on PATH and no pre-built `zig_mod` artifact is staged; \
             host ECS setup verified cleanly."
        );
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
    println!(
        "[HOST] Loaded Zig mod {:?} from {}",
        mod_id,
        dylib_path.display()
    );

    // 2 ticks with dt = 0.25 -> delta = (4.0, -2.0, 6.0)
    app.update();
    app.update();

    let pos = *app.world().get::<Position>(e0).expect("e0 Position");
    println!("  After 2 ticks -> Entity 0: {pos:?}");
    assert_eq!(pos, Position::new(5.0, 0.0, 9.0));

    // Trigger Zig error union mapping (`error.InvalidArgument -> BAM_RESULT_INVALID_ARGUMENT`)
    app.world_mut().resource_mut::<ZigSimConfig>().trigger_error = FfiBool::TRUE;
    app.update();

    let faults = app.world().resource::<Messages<ModSystemFaulted>>();
    assert_eq!(faults.len(), 1);
    let mut cursor = faults.get_cursor();
    let fault = cursor.read(faults).next().expect("ModSystemFaulted");
    assert_eq!(fault.status, FfiResult::INVALID_ARGUMENT);
    println!(
        "  [Zig Error Mapped] system='{}' status={} (quarantined cleanly)",
        fault.system_name, fault.status
    );

    app.unload_mod(mod_id).expect("unload Zig guest mod");
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
    println!("[HOST] Unloaded Zig guest mod cleanly.");
}
