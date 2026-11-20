#![forbid(unsafe_code)]
//! Integration test for Example 08 (`zig_mod/src/root.zig`).
//!
//! Runs full end-to-end Zig guest loading, chunk iteration, error-union mapping, and unload when
//! `zig` is installed (e.g., CI via `goto-bus-stop/setup-zig`), and verifies host registration
//! cleanly when `zig` is absent on a local machine.

use bevy_abi_mod_sys::{FfiBool, FfiOption, FfiResult, ModComponent as ModComponentTrait};
use bevy_app::App;
use bevy_ecs::message::Messages;
use bevy_mod_abi::prelude::*;
use example_08_zig_guest::{
    Position, Velocity, ZigActorStats, ZigBehaviorMode, ZigSimConfig, ZigTelemetry,
    render_example_zig_components, try_resolve_or_build_zig_mod,
};
use std::path::PathBuf;

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

#[test]
fn generated_zig_components_match_mod_component_zig_decl() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let zig_gen_path = manifest_dir.join("zig_mod/src/generated_components.zig");
    let expected = render_example_zig_components();
    if std::env::var_os("BAM_REGEN_EXAMPLE_HEADERS").is_some() {
        std::fs::write(&zig_gen_path, &expected).expect("write generated_components.zig");
    }
    let actual = std::fs::read_to_string(&zig_gen_path)
        .expect("read generated_components.zig")
        .replace("\r\n", "\n");
    assert_eq!(actual, expected);

    assert!(ZigActorStats::ZIG_DECL.contains("ZigActorStatsView"));
    assert!(ZigActorStats::ZIG_DECL.contains("ZigActorStatsVTable_scores_VecView"));
    assert!(ZigActorStats::ZIG_DECL.contains("ZigActorStats_bonus_Option"));
    assert!(ZigBehaviorMode::ZIG_DECL.contains("ZigBehaviorModeTag"));
    assert!(ZigBehaviorMode::ZIG_DECL.contains("ZigBehaviorModeView"));
    assert!(ZigBehaviorMode::ZIG_DECL.contains("resolveVTable"));
}

#[test]
fn zig_guest_typed_views_host_vec_ffi_option_and_enum_roundtrip_or_skip() {
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
    app.register_mod_component::<ZigActorStats>(ComponentOptions::new().with_default())
        .expect("register ZigActorStats");
    app.register_mod_component::<ZigBehaviorMode>(ComponentOptions::new().with_default())
        .expect("register ZigBehaviorMode");
    app.init_resource::<ZigSimConfig>();
    app.register_mod_resource::<ZigSimConfig>()
        .expect("register ZigSimConfig");

    let entity = app
        .world_mut()
        .spawn((
            Position::new(0.0, 0.0, 0.0),
            Velocity::new(4.0, 0.0, 0.0),
            ZigActorStats {
                active: false,
                label: String::from("init"),
                scores: vec![10],
                bonus: FfiOption::none(),
            },
            ZigBehaviorMode::Idle,
        ))
        .id();

    let Some(dylib_path) = try_resolve_or_build_zig_mod() else {
        eprintln!("Skipping Zig typed view load test because `zig` is not installed on PATH.");
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
    let stats = app.world().get::<ZigActorStats>(entity).unwrap();
    assert!(stats.active);
    assert_eq!(stats.label, "zig_view_updated");
    assert_eq!(stats.scores, vec![11, 43]);
    assert_eq!(stats.bonus, FfiOption::some(321));
    assert_eq!(
        *app.world().get::<ZigBehaviorMode>(entity).unwrap(),
        ZigBehaviorMode::Patrol {
            speed: 9.5,
            aggressive: true
        }
    );

    app.unload_mod(mod_id).expect("unload Zig guest mod");
}

#[test]
fn zig_guest_has_filter_lifecycle_observer_hierarchy_and_batch_spawn_or_skip() {
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
    app.register_mod_component::<ZigActorStats>(ComponentOptions::new().with_default())
        .expect("register ZigActorStats");
    app.register_mod_component::<ZigBehaviorMode>(ComponentOptions::new().with_default())
        .expect("register ZigBehaviorMode");
    app.insert_resource(ZigTelemetry {
        spawn_batch_count: 3,
        ..Default::default()
    });
    app.register_mod_resource::<ZigTelemetry>()
        .expect("register ZigTelemetry");

    let parent = app.world_mut().spawn(Position::new(0.0, 0.0, 0.0)).id();
    let child_without_stats = app
        .world_mut()
        .spawn((
            Position::new(1.0, 0.0, 0.0),
            bevy_ecs::hierarchy::ChildOf(parent),
        ))
        .id();

    let Some(dylib_path) = try_resolve_or_build_zig_mod() else {
        eprintln!(
            "Skipping Zig runtime test because `zig` is not installed on PATH (verified host ZigTelemetry registration)."
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

    // Spawn a `ZigActorStats` entity after load so `BAM_EVENT_KIND_ADD` lifecycle observer fires.
    let _observed_actor = app
        .world_mut()
        .spawn((
            Position::new(2.0, 0.0, 0.0),
            ZigActorStats::default(),
            ZigBehaviorMode::Idle,
        ))
        .id();

    // Tick 1: sees 3 Position entities (1 with ZigActorStats, 2 without), 1 ChildOf, 1 Add<ZigActorStats>,
    // and queues `cmd_spawn_batch` of 3 Position entities.
    app.update();
    let t1 = *app.world().resource::<ZigTelemetry>();
    assert_eq!(t1.observed_adds, 1);
    assert_eq!(t1.entities_with_stats, 1);
    assert_eq!(t1.entities_without_stats, 2);
    assert_eq!(t1.child_entities_seen, 1);
    assert_eq!(t1.batch_spawned, FfiBool::TRUE);

    // Tick 2: includes the 3 newly batch-spawned Position entities (now 5 without ZigActorStats).
    app.update();
    let t2 = *app.world().resource::<ZigTelemetry>();
    assert_eq!(t2.entities_with_stats, 1);
    assert_eq!(t2.entities_without_stats, 5);
    assert!(app.world().get::<Position>(child_without_stats).is_some());

    app.unload_mod(mod_id).expect("unload Zig guest mod");
}
