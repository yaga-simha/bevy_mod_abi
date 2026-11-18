//! Hot-reload cycling, schedule system-set excision, shadow-copy loader,
//! asymmetric unload interleaving, post-unload execution/churn safety, and concurrent multi-App lifecycle stress.

use crate::support::{
    C_MOD_CUSTOM_DATA_NAME, CModCustomData, CombatHealth, NameTag, Position,
    RUST_MOD_CUSTOM_DATA_NAME, RustModCustomData, TempDylibCopy, Velocity, ensure_guest_mods_built,
};
use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::GuestSystem;
use bevy_abi_mod_sys::BamSystemCtx;
use bevy_app::{App, Update};
use bevy_ecs::schedule::{ScheduleCleanupPolicy, Schedules};
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use libloading::{Library, Symbol};
use std::sync::Arc;
use std::thread;

unsafe extern "C" fn count_runs_cb(_ctx: *mut BamSystemCtx, user_data: *mut c_void) -> FfiResult {
    // SAFETY: `user_data` points to an `Arc`-owned `AtomicUsize` outliving the `App`.
    unsafe { (*(user_data as *const AtomicUsize)).fetch_add(1, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn hot_reload_1000_cycles_remove_systems_in_set_keeps_schedule_graph_constant() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    #[cfg(not(miri))]
    const CYCLES: u64 = 1000;
    #[cfg(miri)]
    const CYCLES: u64 = 10;

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let reg = app.world().resource::<ModRegistry>().clone();
    let run_counter = Arc::new(AtomicUsize::new(0));

    for cycle in 1..=CYCLES {
        let mod_id = ModId(cycle);
        let sys = GuestSystem::new(
            format!("hot_sys_{cycle}"),
            vec![],
            count_runs_cb,
            Arc::as_ptr(&run_counter) as *mut c_void,
            reg.clone(),
        )
        .with_mod_id(mod_id);

        app.add_systems(Update, sys);
        app.update();

        app.world_mut()
            .resource_scope::<Schedules, _>(|world, mut schedules| {
                let update_sched = schedules.get_mut(Update).unwrap();
                let removed = update_sched
                    .remove_systems_in_set(
                        ModSystemSet::Mod(mod_id),
                        world,
                        ScheduleCleanupPolicy::RemoveSetAndSystems,
                    )
                    .expect("ModSystemSet::Mod(id) must be cleanly removable");
                assert_eq!(removed, 1);
            });
    }

    assert_eq!(run_counter.load(Ordering::SeqCst), CYCLES as usize);

    app.update();
    assert_eq!(run_counter.load(Ordering::SeqCst), CYCLES as usize);

    let schedules = app.world().resource::<Schedules>();
    let update_sched = schedules.get(Update).unwrap();
    assert_eq!(
        update_sched.systems_len(),
        0,
        "{CYCLES} hot-reload cycles must leave 0 dead guest systems in Bevy's Update schedule"
    );
}

#[test]
fn test_50_cycle_hot_reload_lifecycle_stress() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

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

    for cycle in 0..CYCLES {
        let r_id = app
            .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
            .expect("load rust mod in cycle");
        let c_id = app
            .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
            .expect("load c mod in cycle");
        assert_ne!(r_id, c_id);

        for _ in 0..TICKS_PER_CYCLE {
            app.update();
        }

        app.unload_mod(r_id).expect("unload rust mod in cycle");
        app.unload_mod(c_id).expect("unload c mod in cycle");
        assert_eq!(
            app.world().resource::<ModHost>().len(),
            0,
            "Cycle {cycle}: ModHost must be empty after unloading both mods"
        );

        for _ in 0..POST_UNLOAD_TICKS {
            app.update();
        }
    }

    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(pos.y, 300.0);
        assert_eq!(pos.z, 150.0);
        count += 1;
    }
    assert_eq!(count, NUM_ENTITIES);
}

#[test]
fn test_post_unload_execution_safety() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

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

    let r_id = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .unwrap();
    let c_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .unwrap();

    for _ in 0..10 {
        app.update();
    }

    {
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, 20.0);
            assert_eq!(pos.z, 10.0);
        }
    }

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();

    for _ in 0..30 {
        app.update();
    }

    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 10.0);
        count += 1;
    }
    assert_eq!(count, ENTITIES);
}

#[test]
fn test_world_consistency_after_guest_failure() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (shared_rust_dylib, c_dylib) = ensure_guest_mods_built();
    // Private copy so mutating `PANIC_TRIGGER` never races with parallel tests.
    let isolated_rust = TempDylibCopy::new(&shared_rust_dylib, "world_consistency_panic");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<CombatHealth>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<NameTag>(ComponentOptions::new().with_default())
        .unwrap();

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

    let r_id = app
        .load_mod_with(
            isolated_rust.path(),
            LoadOptions::new().with_shadow_copy(false),
        )
        .unwrap();
    let c_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .unwrap();

    for _ in 0..5 {
        app.update();
    }

    // SAFETY: Opening our private `TempDylibCopy` to trigger one isolated `panic = "unwind"` fault.
    unsafe {
        let lib = Library::new(isolated_rust.path()).expect("open isolated rust dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("resolve trigger");
        trigger();
    }

    app.update();

    for &e in &spawned_entities[0..200] {
        app.world_mut().despawn(e);
    }

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

    for _ in 0..10 {
        app.update();
    }

    let mut query = app.world_mut().query::<&Position>();
    let total_living = query.iter(app.world()).count();
    assert_eq!(total_living, 1100);

    let mut count_checked = 0;
    for pos in query.iter(app.world()) {
        if pos.y < 50.0 {
            assert_eq!(pos.z, 16.0);
            count_checked += 1;
        }
    }
    assert_eq!(count_checked, 800);

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();
}

#[test]
fn test_asymmetric_unload_interleaving_and_unload_all() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

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

    let rust_id = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load rust");
    let c_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load c");

    for _ in 0..5 {
        app.update();
    }

    app.unload_mod(c_id).expect("unload c");
    for _ in 0..5 {
        app.update();
    }
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 5.0);
    }

    app.unload_mod(rust_id).expect("unload rust");
    for _ in 0..5 {
        app.update();
    }
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 5.0);
    }

    let _c_id2 = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("reload c");
    for _ in 0..5 {
        app.update();
    }
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 20.0);
        assert_eq!(pos.z, 10.0);
    }

    let _rust_id2 = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("reload rust");
    for _ in 0..5 {
        app.update();
    }
    {
        let pos = app
            .world_mut()
            .query::<&Position>()
            .single(app.world())
            .unwrap();
        assert_eq!(pos.y, 30.0);
        assert_eq!(pos.z, 15.0);
    }

    app.unload_all_mods();
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
        assert_eq!(pos.y, 30.0);
        assert_eq!(pos.z, 15.0);
    }
}

#[test]
fn test_cdylib_panic_containment_and_continued_execution() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (shared_rust_dylib, c_dylib) = ensure_guest_mods_built();
    // Private copy so `trigger_panic` is 100% isolated under parallel `cargo test`.
    let isolated_rust = TempDylibCopy::new(&shared_rust_dylib, "cdylib_panic_containment");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

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

    let rust_mod_id = app
        .load_mod_with(
            isolated_rust.path(),
            LoadOptions::new().with_shadow_copy(false),
        )
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("Failed to load asymmetric_c_mod");

    for _ in 0..20 {
        app.update();
    }

    // SAFETY: Opening our private `TempDylibCopy` to trigger one isolated guest panic.
    unsafe {
        let lib = Library::new(isolated_rust.path()).expect("Failed to open isolated rust_dylib");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("Failed to resolve asymmetric_rust_mod_trigger_panic");
        trigger();
    }

    app.update();

    for _ in 0..50 {
        app.update();
    }

    let expected_y = 2.0 * 71.0;
    let expected_z = 1.0 * 71.0;

    {
        let mut query = app.world_mut().query::<&Position>();
        let mut count = 0;
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, expected_y);
            assert_eq!(pos.z, expected_z);
            count += 1;
        }
        assert_eq!(count, NUM_ENTITIES);
    }

    app.unload_mod(rust_mod_id).expect("unload rust");
    app.unload_mod(c_mod_id).expect("unload c");
    assert!(app.world().resource::<ModHost>().is_empty());

    for _ in 0..20 {
        app.update();
    }

    {
        let mut query = app.world_mut().query::<&Position>();
        for pos in query.iter(app.world()) {
            assert_eq!(pos.y, expected_y);
            assert_eq!(pos.z, expected_z);
        }
    }
}

#[test]
fn test_massive_post_unload_entity_churn() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

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

    let r_id = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load rust");
    let c_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load c");

    for _ in 0..10 {
        app.update();
    }

    app.unload_mod(r_id).expect("unload rust");
    app.unload_mod(c_id).expect("unload c");

    let frozen_y = 20.0f32;
    let frozen_z = 10.0f32;

    for tick in 0..500 {
        if tick < 50 {
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

    let mut query = app.world_mut().query::<&Position>();
    let mut initial_checked = 0;
    let mut spawned_checked = 0;

    for pos in query.iter(app.world()) {
        if pos.x < 10000.0 {
            assert_eq!(pos.y, frozen_y);
            assert_eq!(pos.z, frozen_z);
            initial_checked += 1;
        } else {
            assert_eq!(pos.y, 0.0);
            assert_eq!(pos.z, 0.0);
            spawned_checked += 1;
        }
    }

    assert_eq!(initial_checked, NUM_INITIAL);
    assert_eq!(spawned_checked, 2500);
}

#[test]
fn test_concurrent_multi_app_threads_load_tick_unload() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

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
            app.register_mod_component::<Position>(ComponentOptions::new().with_default())
                .unwrap();
            app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
                .unwrap();

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

            // Intentionally loads WITHOUT shadow copy across all 8 concurrent OS threads to stress
            // per-World `BamInstance` isolation and refcounted `dlopen`/`dlclose` mutexing!
            let no_shadow = LoadOptions::new().with_shadow_copy(false);
            let r_id = app
                .load_mod_with(&r_path, no_shadow.clone())
                .unwrap_or_else(|e| panic!("Thread {t_idx}: load rust failed: {e:?}"));
            let c_id = app
                .load_mod_with(&c_path, no_shadow)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: load c failed: {e:?}"));

            for _ in 0..TICKS_PER_THREAD {
                app.update();
            }

            let expected_y = 2.0 * (TICKS_PER_THREAD as f32);
            let expected_z = 1.0 * (TICKS_PER_THREAD as f32);

            let mut query = app.world_mut().query::<&Position>();
            for pos in query.iter(app.world()) {
                assert!((pos.y - expected_y).abs() < 1e-4);
                assert!((pos.z - expected_z).abs() < 1e-4);
            }

            app.unload_mod(r_id)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: unload rust: {e:?}"));
            app.unload_mod(c_id)
                .unwrap_or_else(|e| panic!("Thread {t_idx}: unload c: {e:?}"));

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
            .unwrap_or_else(|e| panic!("Thread {t_idx} panicked or crashed: {e:?}"));
    }
}

#[test]
fn test_multilanguage_mod_unload_and_post_unload_update_stress() {
    let _pool_guard = crate::DROP_SLOT_POOL_RWLOCK
        .read()
        .unwrap_or_else(|e| e.into_inner());
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(ComponentOptions::new().with_default())
        .unwrap();
    app.register_mod_component::<Velocity>(ComponentOptions::new().with_default())
        .unwrap();

    let rust_mod_id = app
        .load_mod_with(&rust_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("Failed to load asymmetric_c_mod");

    let reg = app.world().resource::<ModRegistry>().clone();
    let rust_comp_id = reg
        .snapshot()
        .get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
        .unwrap()
        .component_id;
    let c_comp_id = reg
        .snapshot()
        .get_by_name(C_MOD_CUSTOM_DATA_NAME)
        .unwrap()
        .component_id;

    let e_rust = app
        .world_mut()
        .spawn((
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 1.0,
                z: 1.0,
            },
        ))
        .id();

    let e_c = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 10.0,
                z: 10.0,
            },
            Velocity {
                x: 2.0,
                y: 2.0,
                z: 2.0,
            },
        ))
        .id();

    let rust_data = RustModCustomData {
        power: 100.0,
        shield: 10,
    };
    // SAFETY: `rust_comp_id` has layout matching `RustModCustomData`.
    unsafe {
        bevy_ecs::ptr::OwningPtr::make(rust_data, |ptr| {
            app.world_mut()
                .entity_mut(e_rust)
                .insert_by_id(rust_comp_id, ptr);
        });
    }

    let c_data = CModCustomData {
        mana: 50.0,
        level: 5,
    };
    // SAFETY: `c_comp_id` has layout matching `CModCustomData`.
    unsafe {
        bevy_ecs::ptr::OwningPtr::make(c_data, |ptr| {
            app.world_mut().entity_mut(e_c).insert_by_id(c_comp_id, ptr);
        });
    }

    for _ in 0..5 {
        app.update();
    }

    {
        // SAFETY: `rust_comp_id` holds initialized `RustModCustomData`.
        let data_r = unsafe {
            *(app
                .world()
                .entity(e_rust)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data_r.power, 150.0);
        assert_eq!(data_r.shield, 15);

        // SAFETY: `c_comp_id` holds initialized `CModCustomData`.
        let data_c = unsafe {
            *(app
                .world()
                .entity(e_c)
                .get_by_id(c_comp_id)
                .unwrap()
                .as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_c.mana, 75.0);
        assert_eq!(data_c.level, 10);
    }

    app.unload_mod(rust_mod_id).expect("Rust mod unload ok");
    app.unload_mod(c_mod_id).expect("C mod unload ok");

    for _ in 0..10 {
        app.update();
    }

    {
        // SAFETY: `ComponentId` storage preserved across unload.
        let data_r = unsafe {
            *(app
                .world()
                .entity(e_rust)
                .get_by_id(rust_comp_id)
                .unwrap()
                .as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data_r.power, 150.0);

        // SAFETY: `ComponentId` storage preserved across unload.
        let data_c = unsafe {
            *(app
                .world()
                .entity(e_c)
                .get_by_id(c_comp_id)
                .unwrap()
                .as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_c.mana, 75.0);
    }

    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
}
