#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use bevy_app::App;
use common::*;
use host_runtime::{AbiModPlugin, ModAppExt};
use libloading::{Library, Symbol};

// ============================================================================
// Milestone 5: Lifecycle & Hot-Reload Verification Suite
// ============================================================================

/// Test 1: 50-Cycle Continuous Mod Hot-Reload and Registration Stress Test.
/// Stress tests repeated library loading, dynamic system scheduling, execution,
/// teardown hook invocation, and unloading across 50 full cycles.
#[test]
fn test_50_cycle_hot_reload_lifecycle_stress() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    const NUM_ENTITIES: usize = 100;
    for i in 0..NUM_ENTITIES {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ));
    }

    const CYCLES: usize = 50;
    const TICKS_PER_CYCLE: usize = 3;
    const POST_UNLOAD_TICKS: usize = 2;

    for _ in 0..CYCLES {
        // 1. Load both guest mods
        let r_id = app
            .load_mod_tracked(&rust_dylib)
            .expect("load rust mod in cycle");
        let c_id = app.load_mod_tracked(&c_dylib).expect("load c mod in cycle");

        // 2. Execute active ticks
        for _ in 0..TICKS_PER_CYCLE {
            app.update();
        }

        // 3. Unload both guest mods
        app.unload_mod(r_id).expect("unload rust mod in cycle");
        app.unload_mod(c_id).expect("unload c mod in cycle");

        // 4. Execute post-unload ticks (mods are unloaded/inactive)
        for _ in 0..POST_UNLOAD_TICKS {
            app.update();
        }
    }

    // Verify entity positions match exact 50 cycles * 3 active ticks = 150 active ticks:
    // Rust mod adds 2.0 to pos.y per active tick -> 150 * 2.0 = 300.0
    // C mod adds 1.0 to pos.z per active tick -> 150 * 1.0 = 150.0
    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(
            pos.y, 300.0,
            "pos.y must equal 300.0 after 50 cycles of 3 active ticks"
        );
        assert_eq!(
            pos.z, 150.0,
            "pos.z must equal 150.0 after 50 cycles of 3 active ticks"
        );
        count += 1;
    }
    assert_eq!(count, NUM_ENTITIES);
}

/// Test 2: Post-Unload Execution Safety.
/// Verifies that after mods are unloaded and dynamic library handles are fully dropped,
/// subsequent Bevy schedule ticks execute with zero dangling pointer crashes, zero aborts,
/// and that entity components remain completely frozen.
#[test]
fn test_post_unload_execution_safety() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    const ENTITIES: usize = 50;
    for i in 0..ENTITIES {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 0.0,
            },
        ));
    }

    let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
    let c_id = app.load_mod_tracked(&c_dylib).unwrap();

    // Run 10 ticks while mods are active
    for _ in 0..10 {
        app.update();
    }

    // Verify state after 10 ticks:
    // Rust mod adds 2.0 to pos.y per tick -> 10 * 2.0 = 20.0
    // C mod adds 1.0 to pos.z per tick -> 10 * 1.0 = 10.0
    {
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, 20.0);
            assert_eq!(pos.z, 10.0);
        }
    }

    // Unload mods cleanly
    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();

    // Execute 30 additional schedule ticks post-unload
    for _ in 0..30 {
        app.update();
    }

    // Verify entity positions remain completely frozen
    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(
            pos.y, 20.0,
            "pos.y must remain exactly 20.0 across all post-unload ticks"
        );
        assert_eq!(
            pos.z, 10.0,
            "pos.z must remain exactly 10.0 across all post-unload ticks"
        );
        count += 1;
    }
    assert_eq!(count, ENTITIES);
}

/// Test 3: ECS World Consistency After Guest Failure.
/// Verifies that when a guest mod panics and is isolated, the host Bevy World
/// retains complete integrity: entity spawning, despawning, archetype mutations,
/// and query traversal continue operating smoothly.
#[test]
fn test_world_consistency_after_guest_failure() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<NameTag>();

    // Spawn 1,000 entities
    const INITIAL_COUNT: usize = 1_000;
    let mut spawned_entities = Vec::with_capacity(INITIAL_COUNT);
    for i in 0..INITIAL_COUNT {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 2.0,
                    z: 0.0,
                },
                CombatHealth {
                    current: 500,
                    max: 500,
                },
            ))
            .id();
        spawned_entities.push(e);
    }

    let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
    let c_id = app.load_mod_tracked(&c_dylib).unwrap();

    // 5 healthy ticks
    for _ in 0..5 {
        app.update();
    }

    // Trigger guest panic in asymmetric_rust_mod
    unsafe {
        let lib = Library::new(&rust_dylib).expect("open rust dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("resolve trigger");
        trigger();
    }

    // Ticking with panic: host catches PANIC_CAUGHT and disables failing system
    app.update();

    // Host ECS mutations following guest failure:
    // 1. Despawn 200 entities
    for &e in &spawned_entities[0..200] {
        app.world_mut().despawn(e);
    }

    // 2. Spawn 300 new entities with different archetype
    for i in 0..300 {
        app.world_mut().spawn((
            Position {
                x: (INITIAL_COUNT + i) as f32,
                y: 100.0,
                z: 50.0,
            },
            NameTag {
                label: format!("post_failure_entity_{i}"),
            },
        ));
    }

    // 3. Run 10 ticks post-failure
    for _ in 0..10 {
        app.update();
    }

    // Verify surviving entity count:
    // 1000 initial - 200 despawned + 300 new = 1100 entities
    let mut query = app.world_mut().query::<&Position>();
    let total_living = query.iter(app.world()).count();
    assert_eq!(
        total_living, 1100,
        "Bevy World must maintain exact entity consistency after guest failure"
    );

    // Verify C mod kinematics system continued advancing pos.z on living entities:
    // 5 healthy ticks + 1 panic tick + 10 post-failure ticks = 16 ticks for C mod
    let mut count_checked = 0;
    for pos in query.iter(app.world()) {
        if pos.y < 50.0 {
            // Initial surviving entity: pos.z should be 16 * 1.0 = 16.0
            assert_eq!(pos.z, 16.0);
            count_checked += 1;
        }
    }
    assert_eq!(count_checked, 800);

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();
    reset_rust_mod_panic(&rust_dylib);
}
