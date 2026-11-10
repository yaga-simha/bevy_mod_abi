#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentAccessMode, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::App;
use bevy_ecs::component::Component;
use host_runtime::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_next_chunk, AbiModPlugin,
    ComponentAccessDescOwned, DynamicCSystem, ModAppExt, ModHost, ModId, ModRegistry,
};
use libloading::{Library, Symbol};
use mod_derive::ModExposable;
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, PartialEq, ModExposable)]
pub struct NameTag {
    pub label: String,
}

fn get_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();

    let rust_mod_dir = root_dir.join("tests/asymmetric_rust_mod");
    let c_mod_dir = root_dir.join("tests/asymmetric_c_mod");

    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let rust_dylib = rust_mod_dir
        .join("target/release")
        .join(format!("{prefix}asymmetric_rust_mod.{ext}"));
    let c_dylib = c_mod_dir.join(format!("{prefix}asymmetric_c_mod.{ext}"));

    (rust_dylib, c_dylib)
}

static BUILD_ONCE: std::sync::Once = std::sync::Once::new();

fn ensure_mods_built() -> (PathBuf, PathBuf) {
    let (rust_dylib, c_dylib) = get_dylib_paths();
    if rust_dylib.exists() && c_dylib.exists() {
        return (rust_dylib, c_dylib);
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();

    BUILD_ONCE.call_once(|| {
        let status_rust = Command::new("cargo")
            .arg("build")
            .arg("--manifest-path")
            .arg(root_dir.join("tests/asymmetric_rust_mod/Cargo.toml"))
            .arg("--release")
            .status()
            .expect("Failed to execute cargo build for asymmetric_rust_mod");
        assert!(
            status_rust.success(),
            "cargo build for asymmetric_rust_mod failed"
        );

        let status_c = Command::new("make")
            .arg("-C")
            .arg(root_dir.join("tests/asymmetric_c_mod"))
            .status()
            .expect("Failed to execute make for asymmetric_c_mod");
        assert!(status_c.success(), "make for asymmetric_c_mod failed");
    });

    let (rust_dylib, c_dylib) = get_dylib_paths();
    assert!(
        rust_dylib.exists(),
        "Rust mod dylib missing: {:?}",
        rust_dylib
    );
    assert!(c_dylib.exists(), "C mod dylib missing: {:?}", c_dylib);

    (rust_dylib, c_dylib)
}

// ============================================================================
// STRESS TEST 1: RAPID MOD RELOAD AND UNLOAD CYCLING (25 SUCCESSIVE CYCLES)
// ============================================================================

#[test]
fn test_stress_rapid_mod_reload_and_unload_cycling() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    const NUM_ENTITIES: usize = 50;
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
    const TICKS_PER_CYCLE: usize = 4;
    const POST_UNLOAD_TICKS: usize = 3;

    let mut expected_y = 0.0f32;
    let mut expected_z = 0.0f32;

    for cycle in 0..CYCLES {
        // 1. Load both mods
        let rust_mod_id = app
            .load_mod_tracked(&rust_dylib)
            .unwrap_or_else(|e| panic!("Cycle {cycle}: Failed to load rust_dylib: {:?}", e));
        let c_mod_id = app
            .load_mod_tracked(&c_dylib)
            .unwrap_or_else(|e| panic!("Cycle {cycle}: Failed to load c_dylib: {:?}", e));

        assert_ne!(rust_mod_id, c_mod_id);

        // 2. Run schedule ticks with mods loaded
        for tick_i in 0..TICKS_PER_CYCLE {
            app.update();
            let sample = app
                .world_mut()
                .query::<&Position>()
                .iter(app.world())
                .next()
                .cloned();
            eprintln!("Cycle {cycle} Tick {tick_i}: sample pos = {:?}", sample);
        }

        expected_y += 2.0 * (TICKS_PER_CYCLE as f32);
        expected_z += 1.0 * (TICKS_PER_CYCLE as f32);

        // Verify position updates match expectations
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert!(
                (pos.y - expected_y).abs() < 1e-4,
                "Cycle {cycle}: pos.y expected {}, got {}",
                expected_y,
                pos.y
            );
            assert!(
                (pos.z - expected_z).abs() < 1e-4,
                "Cycle {cycle}: pos.z expected {}, got {}",
                expected_z,
                pos.z
            );
        }

        // 3. Unload both mods cleanly
        app.unload_mod(rust_mod_id)
            .unwrap_or_else(|e| panic!("Cycle {cycle}: Failed to unload rust mod: {:?}", e));
        app.unload_mod(c_mod_id)
            .unwrap_or_else(|e| panic!("Cycle {cycle}: Failed to unload c mod: {:?}", e));

        // Verify ModHost tracking length is zero
        let host_len = app.world().resource::<ModHost>().len();
        assert_eq!(
            host_len, 0,
            "Cycle {cycle}: ModHost must be empty after unloading all mods"
        );

        // 4. Run schedule ticks while mods are unloaded
        for _ in 0..POST_UNLOAD_TICKS {
            app.update();
        }

        // Verify entity positions remained static (no ghostly updates, no dangling pointers)
        let mut query_unloaded = app.world_mut().query::<&Position>();
        for pos in query_unloaded.iter(app.world()) {
            assert_eq!(
                pos.y, expected_y,
                "Cycle {cycle}: pos.y mutated during post-unload ticks!"
            );
            assert_eq!(
                pos.z, expected_z,
                "Cycle {cycle}: pos.z mutated during post-unload ticks!"
            );
        }
    }

    // 5. Final 20 schedule ticks on clean state
    for _ in 0..20 {
        app.update();
    }

    let mut final_query = app.world_mut().query::<&Position>();
    let mut entity_count = 0;
    for pos in final_query.iter(app.world()) {
        assert_eq!(pos.y, expected_y);
        assert_eq!(pos.z, expected_z);
        entity_count += 1;
    }
    assert_eq!(entity_count, NUM_ENTITIES);
}

// ============================================================================
// STRESS TEST 2: INTERLEAVED ASYMMETRIC UNLOAD & RE-REGISTRATION
// ============================================================================

#[test]
fn test_stress_asymmetric_unload_interleaving() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    app.world_mut().spawn((
        Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        Velocity {
            x: 1.0,
            y: 2.0,
            z: 0.0,
        },
    ));

    // Phase A: Load both
    let rust_id = app.load_mod_tracked(&rust_dylib).expect("load rust");
    let c_id = app.load_mod_tracked(&c_dylib).expect("load c");

    for _ in 0..5 {
        app.update();
    }
    // y = 10.0, z = 5.0

    // Phase B: Unload C only
    app.unload_mod(c_id).expect("unload c");
    for _ in 0..5 {
        app.update();
    }
    // y = 20.0, z remains 5.0
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(
            pos.y, 20.0,
            "Rust kinematics must continue while C is unloaded"
        );
        assert_eq!(pos.z, 5.0, "C kinematics must stop after unload");
    }

    // Phase C: Unload Rust too
    app.unload_mod(rust_id).expect("unload rust");
    for _ in 0..5 {
        app.update();
    }
    // y remains 20.0, z remains 5.0
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 5.0);
    }

    // Phase D: Reload C only
    let _c_id2 = app.load_mod_tracked(&c_dylib).expect("reload c");
    for _ in 0..5 {
        app.update();
    }
    // y remains 20.0, z = 10.0
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 20.0, "Rust must remain stopped");
        assert_eq!(pos.z, 10.0, "Reloaded C mod must resume updating z");
    }

    // Phase E: Reload Rust too
    let _rust_id2 = app.load_mod_tracked(&rust_dylib).expect("reload rust");
    for _ in 0..5 {
        app.update();
    }
    // y = 30.0, z = 15.0
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 30.0);
        assert_eq!(pos.z, 15.0);
    }

    // Phase F: Unload all via ModHost::unload_all
    unsafe {
        app.world_mut().resource_mut::<ModHost>().unload_all();
    }
    assert!(app.world().resource::<ModHost>().is_empty());

    for _ in 0..5 {
        app.update();
    }
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 30.0, "All systems frozen after unload_all");
        assert_eq!(pos.z, 15.0, "All systems frozen after unload_all");
    }
}

// ============================================================================
// STRESS TEST 3: PANIC FLOODING & FAULT CONTAINMENT
// ============================================================================

unsafe extern "C" fn dynamic_panicking_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut core::ffi::c_void,
) -> FfiResult {
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    if flag.load(Ordering::SeqCst) {
        FfiResult::PANIC_CAUGHT
    } else {
        FfiResult::OK
    }
}

unsafe extern "C" fn dynamic_unwinding_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut core::ffi::c_void,
) -> FfiResult {
    let flag = unsafe { &*(user_data as *const AtomicBool) };
    if flag.load(Ordering::SeqCst) {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            panic!("Host-side unwinding stress panic inside dynamic system callback");
        }));
        if res.is_err() {
            return FfiResult::PANIC_CAUGHT;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn dynamic_healthy_counter_callback(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut core::ffi::c_void,
) -> FfiResult {
    let counter = unsafe { &*(user_data as *const AtomicUsize) };
    counter.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_stress_consecutive_and_retriggered_panics() {
    let mut app = App::new();
    let registry = Arc::new(ModRegistry::new());

    let panic_trigger_1 = Arc::new(AtomicBool::new(false));
    let panic_trigger_2 = Arc::new(AtomicBool::new(false));
    let panic_trigger_3 = Arc::new(AtomicBool::new(false));
    let unwinding_trigger = Arc::new(AtomicBool::new(false));
    let healthy_counter = Arc::new(AtomicUsize::new(0));

    // Register 3 panicking systems + 1 unwinding system + 1 healthy system
    let sys1 = DynamicCSystem::new(
        "panic_sys_1".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );
    let sys2 = DynamicCSystem::new(
        "panic_sys_2".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_2) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );
    let sys3 = DynamicCSystem::new(
        "panic_sys_3".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_3) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );
    let sys_unwind = DynamicCSystem::new(
        "unwind_sys".to_string(),
        Vec::new(),
        dynamic_unwinding_callback,
        Arc::as_ptr(&unwinding_trigger) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );
    let sys_healthy = DynamicCSystem::new(
        "healthy_sys".to_string(),
        Vec::new(),
        dynamic_healthy_counter_callback,
        Arc::as_ptr(&healthy_counter) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );

    app.add_systems(
        bevy_app::Update,
        (sys1, sys2, sys3, sys_unwind, sys_healthy),
    );

    // Tick 0-4: All systems healthy
    for _ in 0..5 {
        app.update();
    }
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 5);

    // Trigger consecutive faults one by one:
    // Tick 5: Trigger panic in sys1
    panic_trigger_1.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 6);

    // Tick 6: Trigger panic in sys2
    panic_trigger_2.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 7);

    // Tick 7: Trigger panic in sys3
    panic_trigger_3.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 8);

    // Tick 8: Trigger native unwind in sys_unwind
    unwinding_trigger.store(true, Ordering::SeqCst);
    app.update();
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 9);

    // Run 50 more ticks: all 4 faulty systems remain disabled, healthy system runs every tick
    for _ in 0..50 {
        app.update();
    }
    assert_eq!(healthy_counter.load(Ordering::SeqCst), 59);

    // Retrigger torture: Test re-enabling a system that immediately panics again
    let mut retrigger_sys = DynamicCSystem::new(
        "retrigger_sys".to_string(),
        Vec::new(),
        dynamic_panicking_callback,
        Arc::as_ptr(&panic_trigger_1) as *mut core::ffi::c_void,
        Arc::clone(&registry),
    );

    let mut world = bevy_ecs::world::World::new();
    bevy_ecs::system::System::initialize(&mut retrigger_sys, &mut world);

    // Run 30 cycles of re-enabling an intentionally panicking system
    for _ in 0..30 {
        assert!(retrigger_sys.is_enabled());
        let _ = unsafe {
            bevy_ecs::system::System::run_unsafe(
                &mut retrigger_sys,
                (),
                world.as_unsafe_world_cell(),
            )
        };
        assert!(
            !retrigger_sys.is_enabled(),
            "System must self-disable on panic"
        );
        // Re-enable for torture stress
        retrigger_sys.set_enabled(true);
    }
}

// ============================================================================
// STRESS TEST 4: MULTI-THREADED CONCURRENT SCHEDULE EXECUTION (8 THREADS)
// ============================================================================

#[test]
fn test_stress_concurrent_schedule_threads() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    const NUM_THREADS: usize = 8;
    const TICKS_PER_THREAD: usize = 40;
    const NUM_ENTITIES_PER_APP: usize = 100;

    let mut handles = Vec::new();

    for t_idx in 0..NUM_THREADS {
        let r_path = rust_dylib.clone();
        let c_path = c_dylib.clone();

        let handle = thread::spawn(move || {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.register_mod_component::<Position>();
            app.register_mod_component::<Velocity>();

            for i in 0..NUM_ENTITIES_PER_APP {
                app.world_mut().spawn((
                    Position {
                        x: (t_idx * 1000 + i) as f32,
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

            let r_id = app
                .load_mod_tracked(&r_path)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: load rust failed: {:?}", e));
            let c_id = app
                .load_mod_tracked(&c_path)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: load c failed: {:?}", e));

            for _ in 0..TICKS_PER_THREAD {
                app.update();
            }

            // Verify mutations
            let expected_y = 2.0 * (TICKS_PER_THREAD as f32);
            let expected_z = 1.0 * (TICKS_PER_THREAD as f32);

            let mut query = app.world_mut().query::<&Position>();
            for pos in query.iter(app.world()) {
                assert!(
                    (pos.y - expected_y).abs() < 1e-4,
                    "Thread {t_idx}: expected y {}, got {}",
                    expected_y,
                    pos.y
                );
                assert!(
                    (pos.z - expected_z).abs() < 1e-4,
                    "Thread {t_idx}: expected z {}, got {}",
                    expected_z,
                    pos.z
                );
            }

            // Unload
            app.unload_mod(r_id)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: unload rust: {:?}", e));
            app.unload_mod(c_id)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: unload c: {:?}", e));

            // Post-unload ticks
            for _ in 0..10 {
                app.update();
            }

            let mut post_query = app.world_mut().query::<&Position>();
            for pos in post_query.iter(app.world()) {
                assert_eq!(pos.y, expected_y);
                assert_eq!(pos.z, expected_z);
            }
        });

        handles.push(handle);
    }

    for (t_idx, handle) in handles.into_iter().enumerate() {
        handle
            .join()
            .unwrap_or_else(|e| panic!("Thread {t_idx} panicked or crashed: {:?}", e));
    }
}

// ============================================================================
// STRESS TEST 5: HIGH-SCALE MULTI-ARCHETYPE TOPOLOGY (10,000 ENTITIES)
// ============================================================================

#[test]
fn test_stress_high_scale_multi_archetype() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<NameTag>();

    const PER_ARCHETYPE: usize = 2500;

    // Archetype 1: Position + Velocity
    for i in 0..PER_ARCHETYPE {
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

    // Archetype 2: Position + Velocity + NameTag
    for i in 0..PER_ARCHETYPE {
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
            NameTag {
                label: format!("entity_named_{i}"),
            },
        ));
    }

    // Archetype 3: Position + NameTag (NO Velocity!)
    for i in 0..PER_ARCHETYPE {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            NameTag {
                label: format!("entity_static_{i}"),
            },
        ));
    }

    // Archetype 4: Position (bare)
    for i in 0..PER_ARCHETYPE {
        app.world_mut().spawn((Position {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        },));
    }

    // Load both mods
    let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust");
    let c_id = app.load_mod_tracked(&c_dylib).expect("load c");

    const TICKS: usize = 50;
    for _ in 0..TICKS {
        app.update();
    }

    // Expected values:
    // Rust kinematics queries (Position WRITE, Velocity READ) -> applies to Archetypes 1 and 2 (5,000 entities)
    // C kinematics queries (Position WRITE) -> applies to ALL 4 Archetypes (10,000 entities)
    let mut query = app.world_mut().query::<(Option<&Velocity>, &Position)>();
    let mut vel_count = 0;
    let mut no_vel_count = 0;

    for (maybe_vel, pos) in query.iter(app.world()) {
        if maybe_vel.is_some() {
            // Updated by both Rust and C mods
            assert_eq!(
                pos.y,
                (2.0 * TICKS as f32),
                "pos.y must be updated by Rust mod"
            );
            assert_eq!(
                pos.z,
                (1.0 * TICKS as f32),
                "pos.z must be updated by C mod"
            );
            vel_count += 1;
        } else {
            // Updated ONLY by C mod (pos.y untouched at 0.0)
            assert_eq!(pos.y, 0.0, "pos.y must NOT be touched without Velocity");
            assert_eq!(
                pos.z,
                (1.0 * TICKS as f32),
                "pos.z must be updated by C mod"
            );
            no_vel_count += 1;
        }
    }

    assert_eq!(vel_count, 5000);
    assert_eq!(no_vel_count, 5000);

    // Unload and verify clean post-unload ticks
    app.unload_mod(r_id).expect("unload rust");
    app.unload_mod(c_id).expect("unload c");

    for _ in 0..10 {
        app.update();
    }

    let mut query_post = app.world_mut().query::<(Option<&Velocity>, &Position)>();
    for (maybe_vel, pos) in query_post.iter(app.world()) {
        if maybe_vel.is_some() {
            assert_eq!(pos.y, 100.0);
            assert_eq!(pos.z, 50.0);
        } else {
            assert_eq!(pos.y, 0.0);
            assert_eq!(pos.z, 50.0);
        }
    }
}

// ============================================================================
// STRESS TEST 6: INVALID LIFECYCLE & DEFENSIVE BOUNDARY CONDITIONS
// ============================================================================

#[test]
fn test_stress_invalid_lifecycle_operations() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // 1. Unloading non-existent ModId must safely return Err(INTERNAL_ERROR) without panic
    let err_unload = app.unload_mod(ModId(99999));
    assert_eq!(err_unload, Err(FfiResult::INTERNAL_ERROR));

    let err_unload_zero = app.unload_mod(ModId(0));
    assert_eq!(err_unload_zero, Err(FfiResult::INTERNAL_ERROR));

    // 2. Loading non-existent path must return Err(INTERNAL_ERROR)
    let err_load = app.load_mod_tracked("non_existent_library_path.dylib");
    assert_eq!(err_load, Err(FfiResult::INTERNAL_ERROR));

    // 3. ModHost unload_all on empty host is safe no-op
    unsafe {
        app.world_mut().resource_mut::<ModHost>().unload_all();
    }
    assert!(app.world().resource::<ModHost>().is_empty());
}

// ============================================================================
// STRESS TEST 7: COMPILED CDYLIB PANIC CONTAINMENT & CONTINUED EXECUTION
// ============================================================================

#[test]
fn test_stress_cdylib_panic_containment_and_continued_execution() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    const NUM_ENTITIES: usize = 200;
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

    // 1. Load both mods
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    // 2. Run 20 ticks in healthy state
    for _ in 0..20 {
        app.update();
    }

    // Verify initial updates
    {
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, 40.0);
            assert_eq!(pos.z, 20.0);
        }
    }

    // 3. Trigger intentional panic inside the compiled Rust cdylib
    unsafe {
        let lib = Library::new(&rust_dylib).expect("Failed to open rust_dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("Failed to resolve asymmetric_rust_mod_trigger_panic");
        trigger();
    }

    // 4. Tick with panic triggered: fault is caught inside guest runtime, returning PANIC_CAUGHT.
    // Host isolates and disables rust_panicking without crashing the host Bevy engine.
    app.update();

    // 5. Run 50 more ticks: healthy systems (rust_kinematics and c_kinematics) must continue
    for _ in 0..50 {
        app.update();
    }

    // Total ticks for kinematics = 20 + 1 (panic tick) + 50 = 71 ticks
    let expected_y = 2.0 * 71.0;
    let expected_z = 1.0 * 71.0;

    {
        let mut query = app.world_mut().query::<&Position>();
        let mut count = 0;
        for pos in query.iter(app.world()) {
            assert_eq!(
                pos.y, expected_y,
                "Healthy systems must continue updating pos.y"
            );
            assert_eq!(
                pos.z, expected_z,
                "Healthy systems must continue updating pos.z"
            );
            count += 1;
        }
        assert_eq!(count, NUM_ENTITIES);
    }

    // 6. Hot-unload both mods cleanly
    app.unload_mod(rust_mod_id).expect("unload rust");
    app.unload_mod(c_mod_id).expect("unload c");
    assert!(app.world().resource::<ModHost>().is_empty());

    // 7. Run 20 post-unload ticks: zero crashes, zero mutations
    for _ in 0..20 {
        app.update();
    }

    {
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, expected_y, "Post-unload pos.y must remain frozen");
            assert_eq!(pos.z, expected_z, "Post-unload pos.z must remain frozen");
        }
    }
}

// ============================================================================
// STRESS TEST 8: MASSIVE POST-UNLOAD CHURN & ZERO DANGLING DEREFERENCES
// ============================================================================

#[test]
fn test_stress_massive_post_unload_churn() {
    let (rust_dylib, c_dylib) = ensure_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    const NUM_INITIAL: usize = 500;
    for i in 0..NUM_INITIAL {
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

    let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust");
    let c_id = app.load_mod_tracked(&c_dylib).expect("load c");

    for _ in 0..10 {
        app.update();
    }

    // Unload both mods: shared libraries dropped, is_active set to false
    app.unload_mod(r_id).expect("unload rust");
    app.unload_mod(c_id).expect("unload c");

    let frozen_y = 20.0f32;
    let frozen_z = 10.0f32;

    // Churn the world with 500 schedule ticks and 2,500 new entities
    for tick in 0..500 {
        if tick < 50 {
            // Spawn 50 entities per tick for first 50 ticks (2500 total)
            for j in 0..50 {
                app.world_mut().spawn((
                    Position {
                        x: (10000 + tick * 50 + j) as f32,
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
        }
        app.update();
    }

    // Verify initial 500 entities remained completely frozen at 20.0 and 10.0
    // and newly spawned entities remained untouched at 0.0
    let mut query = app.world_mut().query::<&Position>();
    let mut initial_checked = 0;
    let mut spawned_checked = 0;

    for pos in query.iter(app.world()) {
        if pos.x < 10000.0 {
            assert_eq!(pos.y, frozen_y, "Initial entity pos.y must remain frozen");
            assert_eq!(pos.z, frozen_z, "Initial entity pos.z must remain frozen");
            initial_checked += 1;
        } else {
            assert_eq!(
                pos.y, 0.0,
                "Newly spawned entity pos.y must not be touched by unloaded mods"
            );
            assert_eq!(
                pos.z, 0.0,
                "Newly spawned entity pos.z must not be touched by unloaded mods"
            );
            spawned_checked += 1;
        }
    }

    assert_eq!(initial_checked, NUM_INITIAL);
    assert_eq!(spawned_checked, 2500);
}

// ============================================================================
// STRESS TEST 9: DISJOINT DYNAMIC SYSTEMS MULTI-THREADED PARALLEL EXECUTION
// ============================================================================

unsafe extern "C" fn sys_pos_kinematics(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut core::ffi::c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
        let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
        if pos_ptr.is_null() || vel_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..count {
            unsafe {
                let p = &mut *pos_ptr.add(i);
                let v = &*vel_ptr.add(i);
                p.x += v.x;
                p.y += v.y;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn sys_tag_mutator(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut core::ffi::c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        let tag_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut NameTag };
        if tag_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        for i in 0..count {
            unsafe {
                let tag = &mut *tag_ptr.add(i);
                tag.label.push('!');
            }
        }
    }
    FfiResult::OK
}

#[test]
fn test_stress_disjoint_systems_multi_threaded_parallel_execution() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<NameTag>();

    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());

    // System A queries (Position WRITE, Velocity READ)
    let queries_a = vec![
        ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];
    let sys_a = DynamicCSystem::new(
        "sys_a_kinematics".to_string(),
        queries_a,
        sys_pos_kinematics,
        core::ptr::null_mut(),
        Arc::clone(&registry),
    );

    // System B queries (NameTag WRITE) - completely disjoint from Position and Velocity!
    let queries_b = vec![ComponentAccessDescOwned {
        name: "NameTag".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];
    let sys_b = DynamicCSystem::new(
        "sys_b_tagger".to_string(),
        queries_b,
        sys_tag_mutator,
        core::ptr::null_mut(),
        Arc::clone(&registry),
    );

    app.add_systems(bevy_app::Update, (sys_a, sys_b));

    const NUM_ENTITIES: usize = 2000;
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
            NameTag {
                label: "agent".to_string(),
            },
        ));
    }

    const TICKS: usize = 30;
    for _ in 0..TICKS {
        app.update();
    }

    let mut query = app.world_mut().query::<(&Position, &NameTag)>();
    let expected_suffix = "!".repeat(TICKS);
    let mut verified = 0;

    for (pos, tag) in query.iter(app.world()) {
        assert_eq!(pos.y, 2.0 * TICKS as f32);
        assert_eq!(tag.label, format!("agent{expected_suffix}"));
        verified += 1;
    }
    assert_eq!(verified, NUM_ENTITIES);
}
