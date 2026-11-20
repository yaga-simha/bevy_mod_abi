#![forbid(unsafe_code)]
//! End-to-end integration tests for Example 07 (`cpp_mod/mod.cpp`, ISO C++17).

use bevy_abi_mod_sys::{FfiBool, FfiOption, FfiResult};
use bevy_app::App;
use bevy_ecs::message::Messages;
use bevy_mod_abi::prelude::*;
use example_07_cpp17_guest::{
    ActorStats, BehaviorMode, CppTelemetry, Position, SimConfig, Velocity,
    render_example_components_header, resolve_or_build_cpp17_mod,
};
use std::path::PathBuf;

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

#[test]
fn example_components_header_matches_mod_component_c_decl() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let header_path = manifest_dir.join("cpp_mod/example_components.h");
    let expected = render_example_components_header();
    if std::env::var_os("BAM_REGEN_EXAMPLE_HEADERS").is_some() {
        std::fs::write(&header_path, &expected).expect("write example_components.h");
    }
    let actual = std::fs::read_to_string(&header_path)
        .expect("read example_components.h")
        .replace("\r\n", "\n");
    assert_eq!(actual, expected);
}

#[test]
fn cpp17_guest_typed_views_host_vec_ffi_option_and_enum_roundtrip() {
    let dylib_path = resolve_or_build_cpp17_mod();
    let mut app = build_configured_app();
    app.register_mod_component::<ActorStats>(ComponentOptions::new().with_default())
        .expect("register ActorStats");
    app.register_mod_component::<BehaviorMode>(ComponentOptions::new().with_default())
        .expect("register BehaviorMode");

    let entity = app
        .world_mut()
        .spawn((
            Position::new(0.0, 0.0, 0.0),
            Velocity::new(2.0, 0.0, 0.0),
            ActorStats {
                active: false,
                tag: String::from("initial"),
                scores: vec![10, 20],
                bonus: FfiOption::none(),
            },
            BehaviorMode::Idle,
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
        .expect("load C++17 mod");

    // Tick 1: Idle -> Patrol { speed: 14.5, aggressive: true }, scores [10, 20] -> push(50), push(60), pop(60), +5 -> [15, 25, 55]
    app.update();
    let stats = app.world().get::<ActorStats>(entity).unwrap();
    assert!(stats.active);
    assert_eq!(stats.tag, "cpp17_view_updated");
    assert_eq!(stats.scores, vec![15, 25, 55]);
    assert_eq!(stats.bonus, FfiOption::some(777));
    assert_eq!(
        *app.world().get::<BehaviorMode>(entity).unwrap(),
        BehaviorMode::Patrol {
            speed: 14.5,
            aggressive: true
        }
    );

    // Tick 2: Patrol -> Attack(99), scores [15, 25, 55] -> [20, 30, 60, 55]
    app.update();
    let stats = app.world().get::<ActorStats>(entity).unwrap();
    assert_eq!(stats.scores, vec![20, 30, 60, 55]);
    assert_eq!(
        *app.world().get::<BehaviorMode>(entity).unwrap(),
        BehaviorMode::Attack(99)
    );

    app.unload_mod(mod_id).expect("unload C++17 mod");
}

#[test]
fn cpp17_guest_has_filter_lifecycle_observer_hierarchy_and_batch_spawn() {
    let dylib_path = resolve_or_build_cpp17_mod();
    let mut app = build_configured_app();
    app.register_mod_component::<ActorStats>(ComponentOptions::new().with_default())
        .expect("register ActorStats");
    app.register_mod_component::<BehaviorMode>(ComponentOptions::new().with_default())
        .expect("register BehaviorMode");
    app.insert_resource(CppTelemetry {
        spawn_batch_count: 3,
        ..Default::default()
    });
    app.register_mod_resource::<CppTelemetry>()
        .expect("register CppTelemetry");

    let parent = app.world_mut().spawn(Position::new(0.0, 0.0, 0.0)).id();
    let child_without_stats = app
        .world_mut()
        .spawn((
            Position::new(1.0, 0.0, 0.0),
            bevy_ecs::hierarchy::ChildOf(parent),
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
        .expect("load C++17 mod");

    // Spawn an ActorStats entity after load so `BAM_EVENT_KIND_ADD` lifecycle observer fires.
    let _observed_actor = app
        .world_mut()
        .spawn((
            Position::new(2.0, 0.0, 0.0),
            ActorStats::default(),
            BehaviorMode::Idle,
        ))
        .id();

    // Tick 1: sees 3 Position entities (1 with ActorStats, 2 without), 1 ChildOf, 1 Add<ActorStats>,
    // and queues `cmd_spawn_batch` of 3 Position entities.
    app.update();
    let t1 = *app.world().resource::<CppTelemetry>();
    assert_eq!(t1.observed_adds, 1);
    assert_eq!(t1.entities_with_stats, 1);
    assert_eq!(t1.entities_without_stats, 2);
    assert_eq!(t1.child_entities_seen, 1);
    assert_eq!(t1.batch_spawned, FfiBool::TRUE);

    // Tick 2: includes the 3 newly batch-spawned Position entities (now 5 without ActorStats).
    app.update();
    let t2 = *app.world().resource::<CppTelemetry>();
    assert_eq!(t2.entities_with_stats, 1);
    assert_eq!(t2.entities_without_stats, 5);
    assert!(app.world().get::<Position>(child_without_stats).is_some());

    app.unload_mod(mod_id).expect("unload C++17 mod");
}
