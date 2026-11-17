use crate::support::*;
use bevy_abi_mod_host::{
    AbiModPlugin, ComponentOptions, LoadOptions, ModAppExt, ModQuarantine, ModRegistry,
};
use bevy_abi_mod_host_sys::{
    GuestSystem, HostCommands, HostCommandsStorage, LoadContext, host_cmd_despawn, host_cmd_insert,
    host_cmd_spawn_empty, host_register_component, host_sys_commands, load_mod_in_world,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    BamCommands, BamComponent, BamSystemCtx, ComponentDesc, ComponentUuid, FfiBool, FfiEntity,
    FfiResult, ModComponent as _,
};
use bevy_app::{App, PostUpdate, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{ResMut, Resource, World};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::{LogLevel, MultiThreadedExecutor, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{Query, System};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use libloading::{Library, Symbol};
use std::sync::Arc;

fn shadow_load_options() -> LoadOptions {
    LoadOptions {
        shadow_copy: true,
        ..Default::default()
    }
}

fn remove_shadow_copy_and_parent(path: &std::path::Path) {
    let _ = std::fs::remove_file(path);
    if let Some(parent) = path.parent() {
        let _ = std::fs::remove_dir(parent);
    }
}

#[test]
fn test_asymmetric_mods_concurrency_fault_isolation_and_lifecycle() {
    let (shared_rust_dylib, shared_c_dylib) = ensure_guest_mods_built();
    let rust_dylib = create_shadow_copy(&shared_rust_dylib, "fault_isolated_rust");
    let c_dylib = create_shadow_copy(&shared_c_dylib, "fault_isolated_c");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
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

    // `rust_dylib` and `c_dylib` are already unique per-test shadow copies so `Library::new`
    // below inspects the exact dylib image loaded by `App`.
    let direct_opts = LoadOptions::new().with_shadow_copy(false);
    let rust_mod_id = app
        .load_mod_with(&rust_dylib, direct_opts.clone())
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, direct_opts)
        .expect("Failed to load asymmetric_c_mod");

    for _ in 0..100 {
        app.update();
    }

    let mut query = app.world_mut().query::<&Position>();
    let mut count = 0;
    for pos in query.iter(app.world()) {
        assert_eq!(
            pos.y, 200.0,
            "pos.y must equal 200.0 (2.0 * 100 ticks) from asymmetric_rust_mod"
        );
        assert_eq!(
            pos.z, 100.0,
            "pos.z must equal 100.0 (1.0 * 100 ticks) from asymmetric_c_mod"
        );
        count += 1;
    }
    assert_eq!(count, NUM_ENTITIES);

    // Trigger intentional panic on this isolated copy of `asymmetric_rust_mod`
    unsafe {
        let lib = Library::new(&rust_dylib).expect("Failed to open rust_dylib for panic trigger");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("Failed to resolve asymmetric_rust_mod_trigger_panic");
        trigger();
    }

    // Tick with panic triggered: isolated via PANIC_CAUGHT and quarantined
    app.update();
    assert!(
        app.world()
            .resource::<ModQuarantine>()
            .is_quarantined(rust_mod_id, "rust_panicking"),
        "rust_panicking must be quarantined after cross-runtime panic"
    );
    app.update();

    let mut query = app.world_mut().query::<&Position>();
    for pos in query.iter(app.world()) {
        assert_eq!(
            pos.y, 204.0,
            "pos.y must equal 204.0 (2.0 * 102 ticks); healthy systems continue after fault isolation"
        );
        assert_eq!(
            pos.z, 102.0,
            "pos.z must equal 102.0 (1.0 * 102 ticks); healthy systems continue after fault isolation"
        );
    }

    // Hot-unload both mods cleanly and verify teardown hooks
    unsafe {
        let lib_r = Library::new(&rust_dylib).expect("Failed to open rust_dylib");
        let lib_c = Library::new(&c_dylib).expect("Failed to open c_dylib");

        app.unload_mod(rust_mod_id)
            .expect("Failed to unload asymmetric_rust_mod");
        app.unload_mod(c_mod_id)
            .expect("Failed to unload asymmetric_c_mod");

        let was_shutdown_r: Symbol<unsafe extern "C" fn() -> bool> = lib_r
            .get(b"asymmetric_rust_mod_was_shutdown\0")
            .expect("Failed to resolve asymmetric_rust_mod_was_shutdown");
        assert!(
            was_shutdown_r(),
            "asymmetric_rust_mod shutdown hook must have been executed"
        );

        let was_shutdown_c: Symbol<unsafe extern "C" fn() -> bool> = lib_c
            .get(b"asymmetric_c_mod_was_shutdown\0")
            .expect("Failed to resolve asymmetric_c_mod_was_shutdown");
        assert!(
            was_shutdown_c(),
            "asymmetric_c_mod shutdown hook must have been executed"
        );

        drop(lib_r);
        drop(lib_c);
    }

    // 10 additional ticks post-unload
    for _ in 0..10 {
        app.update();
    }

    let mut query = app.world_mut().query::<&Position>();
    for pos in query.iter(app.world()) {
        assert_eq!(
            pos.y, 204.0,
            "pos.y must remain exactly 204.0 across post-unload ticks"
        );
        assert_eq!(
            pos.z, 102.0,
            "pos.z must remain exactly 102.0 across post-unload ticks"
        );
    }

    remove_shadow_copy_and_parent(&rust_dylib);
    remove_shadow_copy_and_parent(&c_dylib);
}

/// Scenario 2: Multi-Archetype Chunk Iteration with C and Rust Guest Mods Operating Simultaneously.
/// Exercises 10,000 entities partitioned across 4 distinct archetypes.
#[test]
fn test_multi_archetype_chunk_iteration_simultaneous_c_and_rust() {
    let mut builder = TestbedBuilder::new();

    const PER_ARCH: usize = 2_500;
    {
        let world = builder.app.world_mut();
        // Arch 0
        for i in 0..PER_ARCH {
            world.spawn((
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
        // Arch 1
        for i in 0..PER_ARCH {
            world.spawn((
                Position {
                    x: (PER_ARCH + i) as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 2.0,
                    z: 0.0,
                },
                NameTag {
                    label: format!("warrior_{i}"),
                },
            ));
        }
        // Arch 2
        for i in 0..PER_ARCH {
            world.spawn((
                Position {
                    x: (2 * PER_ARCH + i) as f32,
                    y: 0.0,
                    z: 0.0,
                },
                CombatHealth {
                    current: 500,
                    max: 500,
                },
            ));
        }
        // Arch 3
        for i in 0..PER_ARCH {
            world.spawn((
                Position {
                    x: (3 * PER_ARCH + i) as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Mass { value: 10.0 },
            ));
        }
    }

    let r_id = builder
        .app
        .load_mod_with(&builder.rust_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_rust_mod");
    let c_id = builder
        .app
        .load_mod_with(&builder.c_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_c_mod");
    let mut testbed = TestbedApp { app: builder.app };

    testbed.tick(20);

    let mut count_vel = 0;
    let mut vel_query = testbed.world_mut().query::<(&Position, &Velocity)>();
    for (pos, _vel) in vel_query.iter(testbed.world()) {
        assert_eq!(pos.y, 40.0, "pos.y must equal 40.0 from rust kinematics");
        assert_eq!(pos.z, 20.0, "pos.z must equal 20.0 from c kinematics");
        count_vel += 1;
    }
    assert_eq!(count_vel, 2 * PER_ARCH);

    let mut count_mass = 0;
    let mut mass_query = testbed.world_mut().query::<(&Position, &Mass)>();
    for (pos, _mass) in mass_query.iter(testbed.world()) {
        assert_eq!(
            pos.y, 0.0,
            "pos.y must remain 0.0 for entities without Velocity"
        );
        assert_eq!(
            pos.z, 20.0,
            "pos.z must equal 20.0 for Mass entities from c kinematics"
        );
        count_mass += 1;
    }
    assert_eq!(count_mass, PER_ARCH);

    let mut health_query = testbed.world_mut().query::<(&Position, &CombatHealth)>();
    let remaining_health_entities = health_query.iter(testbed.world()).count();
    assert_eq!(
        remaining_health_entities, 0,
        "Combat entities reaching 0 HP must have been despawned via HostCommands"
    );

    testbed.unload_mod(r_id);
    testbed.unload_mod(c_id);
}

#[derive(Resource, Default)]
struct ChangeDetectionTracker {
    changed_positions: Vec<Entity>,
    changed_healths: Vec<Entity>,
}

fn track_changes_system(
    pos_query: Query<Entity, Changed<Position>>,
    health_query: Query<Entity, Changed<CombatHealth>>,
    mut tracker: ResMut<ChangeDetectionTracker>,
) {
    tracker.changed_positions = pos_query.iter().collect();
    tracker.changed_healths = health_query.iter().collect();
}

/// Scenario 3: Change Detection (`Changed<Position>`, `Changed<CombatHealth>`) Verification.
#[test]
fn test_change_detection_position_and_combat_health() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangeDetectionTracker>();
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<CombatHealth>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Mass>(ComponentOptions::new().with_raw_insert().with_default())
        .unwrap();

    let e1 = app
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
                z: 0.0,
            },
        ))
        .id();

    let e2 = app
        .world_mut()
        .spawn(CombatHealth {
            current: 1000,
            max: 1000,
        })
        .id();

    let e3 = app.world_mut().spawn(Mass { value: 42.0 }).id();

    app.add_systems(Update, track_changes_system);

    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangeDetectionTracker>()
            .changed_positions
            .len(),
        1
    );
    assert_eq!(
        app.world()
            .resource::<ChangeDetectionTracker>()
            .changed_healths
            .len(),
        1
    );

    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangeDetectionTracker>()
            .changed_positions
            .len(),
        0,
        "No entities should trigger Changed<Position> before mod execution"
    );
    assert_eq!(
        app.world()
            .resource::<ChangeDetectionTracker>()
            .changed_healths
            .len(),
        0,
        "No entities should trigger Changed<CombatHealth> before mod execution"
    );

    let r_id = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Failed to load rust mod");
    let c_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load c mod");

    app.update();

    let tracker = app.world().resource::<ChangeDetectionTracker>();
    assert!(
        tracker.changed_positions.contains(&e1),
        "e1 must trigger Changed<Position> after mod kinematics mutation"
    );
    assert!(
        !tracker.changed_positions.contains(&e3),
        "e3 (Mass only) must NOT trigger Changed<Position>"
    );
    assert!(
        tracker.changed_healths.contains(&e2),
        "e2 must trigger Changed<CombatHealth> after mod combat mutation"
    );

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();
}

/// Scenario 4: Dynamic Metadata String (`StrView`) Inspection and Mutation.
#[test]
fn test_dynamic_metadata_string_strview_inspection() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<MetadataTag>(ComponentOptions::new().with_default())
        .unwrap();

    let e_grunt = app
        .world_mut()
        .spawn(MetadataTag {
            label: "grunt".to_string(),
        })
        .id();
    let e_knight = app
        .world_mut()
        .spawn(MetadataTag {
            label: "knight".to_string(),
        })
        .id();

    let c_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load c mod");

    app.update();

    let tag_grunt = app.world().get::<MetadataTag>(e_grunt).unwrap();
    assert_eq!(
        tag_grunt.label, "elite_grunt",
        "C mod metadata system must upgrade 'grunt' to 'elite_grunt'"
    );

    let tag_knight = app.world().get::<MetadataTag>(e_knight).unwrap();
    assert_eq!(
        tag_knight.label, "knight",
        "C mod metadata system must leave 'knight' intact"
    );

    app.unload_mod(c_id).unwrap();

    let r_id = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Failed to load rust mod");

    app.update();

    let tag_grunt_after = app.world().get::<MetadataTag>(e_grunt).unwrap();
    assert_eq!(
        tag_grunt_after.label, "Modded_elite_grunt",
        "Rust mod must prepend 'Modded_' to 'elite_grunt'"
    );

    let tag_knight_after = app.world().get::<MetadataTag>(e_knight).unwrap();
    assert_eq!(
        tag_knight_after.label, "Modded_knight",
        "Rust mod must prepend 'Modded_' to 'knight'"
    );

    app.unload_mod(r_id).unwrap();
}

unsafe extern "C" fn custom_command_spawner_callback(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut core::ffi::c_void,
) -> FfiResult {
    let pos_handle = BamComponent::from_raw(user_data as usize as u32);
    let mut commands: *mut BamCommands = core::ptr::null_mut();
    let status = unsafe { host_sys_commands(sys_ctx, &mut commands) };
    if status.is_err() {
        return status;
    }
    let mut spawned = FfiEntity::NULL;
    let spawn_res = unsafe { host_cmd_spawn_empty(commands, &mut spawned) };
    if spawn_res.is_err() {
        return spawn_res;
    }
    let p = Position {
        x: 77.0,
        y: 88.0,
        z: 99.0,
    };
    unsafe {
        host_cmd_insert(
            commands,
            spawned,
            pos_handle,
            (&raw const p).cast::<u8>(),
            core::mem::size_of::<Position>(),
        )
    }
}

/// Scenario 5: Deferred Commands (`spawn_empty`, `insert`, `despawn`) Flush Verification.
#[test]
fn test_deferred_commands_spawn_insert_despawn_flush() {
    let (_rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_handle = app
        .register_mod_component::<Position>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .unwrap();
    app.register_mod_component::<CombatHealth>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    let dying_entity = app
        .world_mut()
        .spawn((
            Position {
                x: 10.0,
                y: 20.0,
                z: 30.0,
            },
            CombatHealth {
                current: 10,
                max: 100,
            },
        ))
        .id();

    let registry = app.world().resource::<ModRegistry>().clone();
    let cmd_system = GuestSystem::new(
        "custom_command_spawner",
        vec![],
        custom_command_spawner_callback,
        pos_handle.to_raw() as usize as *mut c_void,
        registry,
    );
    app.add_systems(Update, cmd_system);

    let c_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load c mod");

    app.update();

    assert!(
        app.world().get_entity(dying_entity).is_err(),
        "dying_entity must be despawned from the Bevy World after apply_deferred"
    );

    let mut query = app.world_mut().query::<&Position>();
    let spawned_found = query
        .iter(app.world())
        .any(|p| p.x == 77.0 && p.y == 88.0 && p.z == 99.0);
    assert!(
        spawned_found,
        "Spawned entity with Position {{ 77.0, 88.0, 99.0 }} must exist in Bevy World"
    );

    app.unload_mod(c_id).unwrap();
}

/// Scenario 6: Multi-Language Dynamic Component Lifecycle & Concurrency Test.
#[test]
fn test_runtime_dynamic_components_multilanguage() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    let rust_mod_id = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_c_mod");

    let (rust_comp_id, c_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();

        let rust_meta = registry
            .get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
            .expect("RustModCustomData must be registered in ModRegistry");
        assert_eq!(rust_meta.size, 8);
        assert_eq!(rust_meta.align, 4);
        assert_eq!(
            rust_meta.uuid,
            ComponentUuid::from_parts(RUST_MOD_CUSTOM_DATA_NAME, 8, 4, 0)
        );
        assert!(rust_meta.is_dynamic);

        let c_meta = registry
            .get_by_name(C_MOD_CUSTOM_DATA_NAME)
            .expect("CModCustomData must be registered in ModRegistry");
        assert_eq!(c_meta.size, 8);
        assert_eq!(c_meta.align, 4);
        assert_eq!(
            c_meta.uuid,
            ComponentUuid::from_parts(C_MOD_CUSTOM_DATA_NAME, 8, 4, 0)
        );
        assert!(c_meta.is_dynamic);

        (rust_meta.component_id, c_meta.component_id)
    };

    let e_rust = {
        let e = app.world_mut().spawn_empty().id();
        let init_data = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        e
    };

    let e_c = {
        let e = app.world_mut().spawn_empty().id();
        let init_data = CModCustomData {
            mana: 25.0,
            level: 3,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        e
    };

    let e_both = {
        let e = app.world_mut().spawn_empty().id();
        let init_rust = RustModCustomData {
            power: 200.0,
            shield: 10,
        };
        let init_c = CModCustomData {
            mana: 50.0,
            level: 7,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_rust, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(init_c, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        e
    };

    let initial_rust_tick = app
        .world()
        .entity(e_rust)
        .get_change_ticks_by_id(rust_comp_id)
        .unwrap()
        .changed;
    let initial_c_tick = app
        .world()
        .entity(e_c)
        .get_change_ticks_by_id(c_comp_id)
        .unwrap()
        .changed;
    let initial_both_rust_tick = app
        .world()
        .entity(e_both)
        .get_change_ticks_by_id(rust_comp_id)
        .unwrap()
        .changed;
    let initial_both_c_tick = app
        .world()
        .entity(e_both)
        .get_change_ticks_by_id(c_comp_id)
        .unwrap()
        .changed;

    app.update();

    let frame1_tick = app.world_mut().change_tick();

    {
        let entity_ref = app.world().entity(e_rust);
        let ptr = entity_ref
            .get_by_id(rust_comp_id)
            .expect("e_rust must have RustModCustomData");
        let data = unsafe { *(ptr.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data,
            RustModCustomData {
                power: 110.0,
                shield: 51,
            }
        );
        let ticks = entity_ref.get_change_ticks_by_id(rust_comp_id).unwrap();
        assert_ne!(ticks.changed, initial_rust_tick);
        assert!(ticks.is_changed(initial_rust_tick, frame1_tick));
    }

    {
        let entity_ref = app.world().entity(e_c);
        let ptr = entity_ref
            .get_by_id(c_comp_id)
            .expect("e_c must have CModCustomData");
        let data = unsafe { *(ptr.as_ptr() as *const CModCustomData) };
        assert_eq!(
            data,
            CModCustomData {
                mana: 30.0,
                level: 4,
            }
        );
        let ticks = entity_ref.get_change_ticks_by_id(c_comp_id).unwrap();
        assert_ne!(ticks.changed, initial_c_tick);
        assert!(ticks.is_changed(initial_c_tick, frame1_tick));
    }

    {
        let entity_ref = app.world().entity(e_both);

        let ptr_rust = entity_ref
            .get_by_id(rust_comp_id)
            .expect("e_both must have RustModCustomData");
        let data_rust = unsafe { *(ptr_rust.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data_rust,
            RustModCustomData {
                power: 210.0,
                shield: 11,
            }
        );

        let ptr_c = entity_ref
            .get_by_id(c_comp_id)
            .expect("e_both must have CModCustomData");
        let data_c = unsafe { *(ptr_c.as_ptr() as *const CModCustomData) };
        assert_eq!(
            data_c,
            CModCustomData {
                mana: 55.0,
                level: 8,
            }
        );

        let ticks_rust = entity_ref.get_change_ticks_by_id(rust_comp_id).unwrap();
        let ticks_c = entity_ref.get_change_ticks_by_id(c_comp_id).unwrap();
        assert_ne!(ticks_rust.changed, ticks_c.changed);
        assert_ne!(ticks_rust.changed, initial_both_rust_tick);
        assert_ne!(ticks_c.changed, initial_both_c_tick);
        assert!(ticks_rust.is_changed(initial_both_rust_tick, frame1_tick));
        assert!(ticks_c.is_changed(initial_both_c_tick, frame1_tick));
    }

    for _ in 0..4 {
        app.update();
    }

    {
        let entity_ref = app.world().entity(e_rust);
        let data = unsafe {
            *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            data,
            RustModCustomData {
                power: 150.0,
                shield: 55
            }
        );

        let entity_ref = app.world().entity(e_c);
        let data = unsafe {
            *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data,
            CModCustomData {
                mana: 50.0,
                level: 8
            }
        );

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe {
            *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data_r,
            RustModCustomData {
                power: 250.0,
                shield: 15
            }
        );
        assert_eq!(
            data_c,
            CModCustomData {
                mana: 75.0,
                level: 12
            }
        );
    }

    app.unload_mod(rust_mod_id)
        .expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id).expect("C mod must unload cleanly");

    app.update();
    app.update();

    {
        let entity_ref = app.world().entity(e_rust);
        let data = unsafe {
            *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            data,
            RustModCustomData {
                power: 150.0,
                shield: 55
            }
        );

        let entity_ref = app.world().entity(e_c);
        let data = unsafe {
            *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data,
            CModCustomData {
                mana: 50.0,
                level: 8
            }
        );

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe {
            *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data_r,
            RustModCustomData {
                power: 250.0,
                shield: 15
            }
        );
        assert_eq!(
            data_c,
            CModCustomData {
                mana: 75.0,
                level: 12
            }
        );
    }

    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
    app.world_mut().despawn(e_both);
}

#[derive(Resource, Default)]
struct DownstreamNativeTracker {
    changed_position_entities: Vec<Entity>,
    tick_count: usize,
}

fn downstream_native_position_observer(
    query: Query<Entity, Changed<Position>>,
    mut tracker: ResMut<DownstreamNativeTracker>,
) {
    tracker.changed_position_entities = query.iter().collect();
    tracker.tick_count += 1;
}

/// High-Throughput Archetype Permutations & Change Detection
#[test]
fn test_high_throughput_archetype_permutations() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    app.init_resource::<DownstreamNativeTracker>();
    app.add_systems(PostUpdate, downstream_native_position_observer);

    let rust_mod_id = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_c_mod");

    let (rust_comp_id, c_comp_id, pos_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();
        let r_meta = registry
            .get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
            .expect("RustModCustomData must be registered");
        let c_meta = registry
            .get_by_name(C_MOD_CUSTOM_DATA_NAME)
            .expect("CModCustomData must be registered");
        let p_meta = registry
            .get_by_name(Position::CANONICAL_NAME)
            .expect("Position must be registered");

        assert!(r_meta.is_dynamic);
        assert!(c_meta.is_dynamic);
        assert_eq!(r_meta.size, 8);
        assert_eq!(c_meta.size, 8);
        (
            r_meta.component_id,
            c_meta.component_id,
            p_meta.component_id,
        )
    };

    const PER_ARCHETYPE: usize = 2_500;
    const TOTAL_ENTITIES: usize = PER_ARCHETYPE * 4;
    let pre_spawn_count = app.world_mut().query::<Entity>().iter(app.world()).count();

    let mut arch1_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch2_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch3_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch4_entities = Vec::with_capacity(PER_ARCHETYPE);

    for i in 0..PER_ARCHETYPE {
        let e = app.world_mut().spawn_empty().id();
        let init_data = RustModCustomData {
            power: 100.0 + (i as f32),
            shield: 10 + ((i % 50) as u32),
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        arch1_entities.push(e);
    }

    for i in 0..PER_ARCHETYPE {
        let e = app.world_mut().spawn_empty().id();
        let init_data = CModCustomData {
            mana: 50.0 + (i as f32),
            level: 1 + ((i % 20) as i32),
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        arch2_entities.push(e);
    }

    for i in 0..PER_ARCHETYPE {
        let e = app.world_mut().spawn_empty().id();
        let init_rust = RustModCustomData {
            power: 200.0 + (i as f32),
            shield: 20 + ((i % 50) as u32),
        };
        let init_c = CModCustomData {
            mana: 100.0 + (i as f32),
            level: 5 + ((i % 20) as i32),
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_rust, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(init_c, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        arch3_entities.push(e);
    }

    for i in 0..PER_ARCHETYPE {
        let e = app
            .world_mut()
            .spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            })
            .id();
        let init_rust = RustModCustomData {
            power: 300.0 + (i as f32),
            shield: 30 + ((i % 50) as u32),
        };
        let init_c = CModCustomData {
            mana: 150.0 + (i as f32),
            level: 10 + ((i % 20) as i32),
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_rust, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(init_c, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        arch4_entities.push(e);
    }

    assert_eq!(
        arch1_entities.len() + arch2_entities.len() + arch3_entities.len() + arch4_entities.len(),
        TOTAL_ENTITIES
    );
    assert_eq!(
        app.world_mut().query::<Entity>().iter(app.world()).count(),
        pre_spawn_count + TOTAL_ENTITIES
    );

    let pre_sim_tick = app.world_mut().change_tick();

    const SIM_TICKS: usize = 10;
    for _ in 1..=SIM_TICKS {
        app.update();
        let tracker = app.world().resource::<DownstreamNativeTracker>();
        assert!(
            tracker.changed_position_entities.len() >= PER_ARCHETYPE,
            "At least {PER_ARCHETYPE} Permutation 4 entities must trigger Changed<Position> in PostUpdate"
        );
    }

    let post_sim_tick = app.world_mut().change_tick();

    for (i, &e) in arch1_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data.power, 100.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data.shield, 10 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert!(ent.get_by_id(c_comp_id).is_err());
        assert!(ent.get_by_id(pos_comp_id).is_err());

        let ticks = ent.get_change_ticks_by_id(rust_comp_id).unwrap();
        assert!(ticks.is_changed(pre_sim_tick, post_sim_tick));
    }

    for (i, &e) in arch2_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data.mana, 50.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data.level, 1 + ((i % 20) as i32) + (SIM_TICKS as i32));
        assert!(ent.get_by_id(rust_comp_id).is_err());
        assert!(ent.get_by_id(pos_comp_id).is_err());

        let ticks = ent.get_change_ticks_by_id(c_comp_id).unwrap();
        assert!(ticks.is_changed(pre_sim_tick, post_sim_tick));
    }

    for (i, &e) in arch3_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data_r =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r.power, 200.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data_r.shield, 20 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert_eq!(data_c.mana, 100.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data_c.level, 5 + ((i % 20) as i32) + (SIM_TICKS as i32));
        assert!(ent.get_by_id(pos_comp_id).is_err());
    }

    for (i, &e) in arch4_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let pos = ent.get::<Position>().unwrap();
        assert_eq!(pos.x, i as f32);
        assert_eq!(pos.y, 0.0);
        assert_eq!(pos.z, SIM_TICKS as f32 * 1.0);

        let data_r =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r.power, 300.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data_r.shield, 30 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert_eq!(data_c.mana, 150.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data_c.level, 10 + ((i % 20) as i32) + (SIM_TICKS as i32));
    }

    app.unload_mod(rust_mod_id)
        .expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id).expect("C mod must unload cleanly");

    for _ in 0..5 {
        app.update();
    }

    let tracker_post = app.world().resource::<DownstreamNativeTracker>();
    assert_eq!(
        tracker_post.changed_position_entities.len(),
        0,
        "Post-unload ticks must produce 0 Changed<Position> events"
    );

    let sample_e4 = arch4_entities[0];
    let pos_frozen = app.world().entity(sample_e4).get::<Position>().unwrap();
    assert_eq!(pos_frozen.z, SIM_TICKS as f32 * 1.0);

    for e in arch1_entities
        .into_iter()
        .chain(arch2_entities)
        .chain(arch3_entities)
        .chain(arch4_entities)
    {
        app.world_mut().despawn(e);
    }
    assert!(app.world().get_entity(sample_e4).is_err());
}

/// Parallel Scheduler Concurrency under MultiThreadedExecutor
#[test]
fn test_parallel_scheduler_concurrency() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    let opts = shadow_load_options();
    let (rust_mod_id, rust_systems) = load_mod_in_world(app.world_mut(), &rust_dylib, &opts)
        .expect("Failed to load asymmetric_rust_mod");

    let (c_mod_id, c_systems) = load_mod_in_world(app.world_mut(), &c_dylib, &opts)
        .expect("Failed to load asymmetric_c_mod");

    let rust_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
        .unwrap()
        .component_id;
    let c_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(C_MOD_CUSTOM_DATA_NAME)
        .unwrap()
        .component_id;

    let mut sys_rust = None;
    for mut s in rust_systems {
        if let bevy_ecs::system::SystemAccess::Shared(set) = s.initialize(app.world_mut())
            && set.combined_access().has_write(rust_comp_id)
        {
            sys_rust = Some(s);
            break;
        }
    }
    let mut sys_rust = sys_rust.expect("rust_custom_system querying RustModCustomData must exist");

    let mut sys_c = None;
    for mut s in c_systems {
        if let bevy_ecs::system::SystemAccess::Shared(set) = s.initialize(app.world_mut())
            && set.combined_access().has_write(c_comp_id)
        {
            sys_c = Some(s);
            break;
        }
    }
    let mut sys_c = sys_c.expect("c_custom_data_system querying CModCustomData must exist");

    let access_rust = sys_rust.initialize(app.world_mut());
    let access_c = sys_c.initialize(app.world_mut());
    assert!(
        access_rust.is_compatible(&access_c),
        "Disjoint dynamic systems must have compatible access for parallel execution"
    );

    let mut schedule = Schedule::default();
    schedule.set_executor(MultiThreadedExecutor::default());
    schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });
    schedule.add_systems((sys_rust, sys_c));

    assert!(
        schedule.initialize(app.world_mut()).is_ok(),
        "Disjoint dynamic systems must initialize in MultiThreadedExecutor with ambiguity_detection: LogLevel::Error"
    );

    const NUM_ENTITIES: usize = 10_000;
    let mut entities = Vec::with_capacity(NUM_ENTITIES);
    for i in 0..NUM_ENTITIES {
        let e = app.world_mut().spawn_empty().id();
        let r_init = RustModCustomData {
            power: (i as f32) * 2.0,
            shield: 100 + ((i % 1000) as u32),
        };
        let c_init = CModCustomData {
            mana: (i as f32) * 0.5,
            level: 10 + ((i % 50) as i32),
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(r_init, |ptr| unsafe {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(c_init, |ptr| unsafe {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        entities.push(e);
    }

    const TICKS: usize = 5;
    for _ in 0..TICKS {
        schedule.run(app.world_mut());
    }

    for (i, &e) in entities.iter().enumerate() {
        let entity_ref = app.world().entity(e);
        let r_ptr = entity_ref.get_by_id(rust_comp_id).unwrap();
        let r_val = unsafe { *(r_ptr.as_ptr() as *const RustModCustomData) };
        assert_eq!(r_val.power, (i as f32) * 2.0 + (TICKS as f32 * 10.0));
        assert_eq!(r_val.shield, 100 + ((i % 1000) as u32) + (TICKS as u32));

        let c_ptr = entity_ref.get_by_id(c_comp_id).unwrap();
        let c_val = unsafe { *(c_ptr.as_ptr() as *const CModCustomData) };
        assert_eq!(c_val.mana, (i as f32) * 0.5 + (TICKS as f32 * 5.0));
        assert_eq!(c_val.level, 10 + ((i % 50) as i32) + (TICKS as i32));
    }

    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
}

/// Deferred Command Arena Burst & Allocator Isolation
#[test]
fn test_deferred_command_queue_burst_and_allocator_isolation() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    let rust_mod_id = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Failed to load asymmetric_c_mod");

    let snapshot = app.world().resource::<ModRegistry>().snapshot_arc();
    let rust_meta = snapshot.get_by_name(RUST_MOD_CUSTOM_DATA_NAME).unwrap();
    let c_meta = snapshot.get_by_name(C_MOD_CUSTOM_DATA_NAME).unwrap();
    let rust_handle = rust_meta.handle;
    let c_handle = c_meta.handle;
    let rust_comp_id = rust_meta.component_id;
    let c_comp_id = c_meta.component_id;

    let mut cmd_storage = HostCommandsStorage::new();
    let (ffi_poison_rust, ffi_poison_c, ffi_despawn_e, burst_entities) = {
        let mut host_cmds =
            HostCommands::new(&mut cmd_storage, app.world().entity_allocator(), &snapshot);
        let raw_queue = host_cmds.as_ffi_ptr();

        // 1. Memory Poisoning Test (Rule 2: Allocator Isolation)
        let mut ffi_poison_rust = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_queue, &mut ffi_poison_rust) },
            FfiResult::OK
        );
        let mut guest_rust_buf = RustModCustomData {
            power: 777.75,
            shield: 999,
        };
        let res_r = unsafe {
            host_cmd_insert(
                raw_queue,
                ffi_poison_rust,
                rust_handle,
                (&raw const guest_rust_buf).cast::<u8>(),
                core::mem::size_of::<RustModCustomData>(),
            )
        };
        assert_eq!(res_r, FfiResult::OK);

        guest_rust_buf.power = -99999.0;
        guest_rust_buf.shield = 0xDEAD_BEEF;
        assert_eq!(guest_rust_buf.power, -99999.0);
        assert_eq!(guest_rust_buf.shield, 0xDEAD_BEEF);

        let mut ffi_poison_c = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_queue, &mut ffi_poison_c) },
            FfiResult::OK
        );
        let mut guest_c_buf = CModCustomData {
            mana: 333.25,
            level: 42,
        };
        let res_c = unsafe {
            host_cmd_insert(
                raw_queue,
                ffi_poison_c,
                c_handle,
                (&raw const guest_c_buf).cast::<u8>(),
                core::mem::size_of::<CModCustomData>(),
            )
        };
        assert_eq!(res_c, FfiResult::OK);

        guest_c_buf.mana = -88888.0;
        guest_c_buf.level = -1;
        assert_eq!(guest_c_buf.mana, -88888.0);
        assert_eq!(guest_c_buf.level, -1);

        // 2. Despawn-Before-Apply Safety Test
        let mut ffi_despawn_e = FfiEntity::NULL;
        assert_eq!(
            unsafe { host_cmd_spawn_empty(raw_queue, &mut ffi_despawn_e) },
            FfiResult::OK
        );
        let dummy_data = RustModCustomData {
            power: 1.0,
            shield: 1,
        };
        unsafe {
            assert_eq!(
                host_cmd_insert(
                    raw_queue,
                    ffi_despawn_e,
                    rust_handle,
                    (&raw const dummy_data).cast::<u8>(),
                    8,
                ),
                FfiResult::OK
            );
            assert_eq!(host_cmd_despawn(raw_queue, ffi_despawn_e), FfiResult::OK);
        }

        // 3. High-Volume Burst Enqueueing (2,000 entities)
        const BURST_COUNT: usize = 2_000;
        let mut burst_entities = Vec::with_capacity(BURST_COUNT);
        for i in 0..BURST_COUNT {
            let mut ffi_e = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn_empty(raw_queue, &mut ffi_e) },
                FfiResult::OK
            );
            assert!(!ffi_e.is_null());
            burst_entities.push(ffi_e);

            if i % 2 == 0 {
                let r_data = RustModCustomData {
                    power: (i as f32) + 0.5,
                    shield: (i as u32) + 10,
                };
                unsafe {
                    assert_eq!(
                        host_cmd_insert(
                            raw_queue,
                            ffi_e,
                            rust_handle,
                            (&raw const r_data).cast::<u8>(),
                            core::mem::size_of::<RustModCustomData>(),
                        ),
                        FfiResult::OK
                    );
                }
            } else {
                let c_data = CModCustomData {
                    mana: (i as f32) * 1.5,
                    level: (i as i32) + 1,
                };
                unsafe {
                    assert_eq!(
                        host_cmd_insert(
                            raw_queue,
                            ffi_e,
                            c_handle,
                            (&raw const c_data).cast::<u8>(),
                            core::mem::size_of::<CModCustomData>(),
                        ),
                        FfiResult::OK
                    );
                }
            }
        }

        (ffi_poison_rust, ffi_poison_c, ffi_despawn_e, burst_entities)
    };

    cmd_storage.apply_to_world(app.world_mut(), &snapshot);

    {
        let ent_r = app.world().entity(Entity::from_bits(ffi_poison_rust.0));
        let ptr_r = ent_r
            .get_by_id(rust_comp_id)
            .expect("Poison test Rust entity exists");
        let data_r = unsafe { *(ptr_r.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data_r,
            RustModCustomData {
                power: 777.75,
                shield: 999,
            }
        );

        let ent_c = app.world().entity(Entity::from_bits(ffi_poison_c.0));
        let ptr_c = ent_c
            .get_by_id(c_comp_id)
            .expect("Poison test C entity exists");
        let data_c = unsafe { *(ptr_c.as_ptr() as *const CModCustomData) };
        assert_eq!(
            data_c,
            CModCustomData {
                mana: 333.25,
                level: 42,
            }
        );
    }

    let bevy_despawn_e = Entity::from_bits(ffi_despawn_e.0);
    assert!(
        app.world().get_entity(bevy_despawn_e).is_err(),
        "Entity despawned before apply must not exist in world"
    );

    for (i, &ffi_e) in burst_entities.iter().enumerate() {
        let bevy_e = Entity::from_bits(ffi_e.0);
        let entity_ref = app.world().entity(bevy_e);
        if i % 2 == 0 {
            let ptr = entity_ref.get_by_id(rust_comp_id).unwrap();
            let data = unsafe { *(ptr.as_ptr() as *const RustModCustomData) };
            assert_eq!(data.power, (i as f32) + 0.5);
            assert_eq!(data.shield, (i as u32) + 10);
        } else {
            let ptr = entity_ref.get_by_id(c_comp_id).unwrap();
            let data = unsafe { *(ptr.as_ptr() as *const CModCustomData) };
            assert_eq!(data.mana, (i as f32) * 1.5);
            assert_eq!(data.level, (i as i32) + 1);
        }
    }

    app.update();

    for (i, &ffi_e) in burst_entities.iter().enumerate() {
        let bevy_e = Entity::from_bits(ffi_e.0);
        let entity_ref = app.world().entity(bevy_e);
        if i % 2 == 0 {
            let ptr = entity_ref.get_by_id(rust_comp_id).unwrap();
            let data = unsafe { *(ptr.as_ptr() as *const RustModCustomData) };
            assert_eq!(data.power, (i as f32) + 0.5 + 10.0);
            assert_eq!(data.shield, (i as u32) + 10 + 1);
        } else {
            let ptr = entity_ref.get_by_id(c_comp_id).unwrap();
            let data = unsafe { *(ptr.as_ptr() as *const CModCustomData) };
            assert_eq!(data.mana, (i as f32) * 1.5 + 5.0);
            assert_eq!(data.level, (i as i32) + 1 + 1);
        }
    }

    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
}

/// Hot-Unload, Reload Lifecycle Safety, Atomic Tombstones & Drop Trampolines
#[test]
fn test_hot_unload_reload_and_tombstone_lifecycle_safety() {
    static DROP_INVOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn mock_foreign_drop_fn(_ptr: *mut c_void) {
        DROP_INVOCATION_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    DROP_INVOCATION_COUNT.store(0, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();

    // Initial mod load
    let rust_mod_id_1 = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Load Rust mod (initial)");
    let c_mod_id_1 = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Load C mod (initial)");

    let (rust_comp_id, c_comp_id) = {
        let reg = app.world().resource::<ModRegistry>();
        (
            reg.get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
            reg.get_by_name(C_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
        )
    };

    // Register a non-POD dynamic component with a foreign drop hook into the drop trampoline pool
    let drop_test_uuid = ComponentUuid::from_canonical_name("testbed::NonPodDropTestComp");
    let non_pod_is_active = Arc::new(AtomicBool::new(true));
    let non_pod_comp_id = {
        let reg_clone = app.world().resource::<ModRegistry>().clone();
        let mut load_ctx = LoadContext::with_world_and_mod_id(
            reg_clone,
            Arc::clone(&non_pod_is_active),
            app.world_mut() as *mut World,
            None,
        );
        let desc = ComponentDesc::new(
            c"testbed::NonPodDropTestComp".as_ptr(),
            drop_test_uuid,
            8,
            4,
        )
        .with_drop(mock_foreign_drop_fn);
        let mut out_comp = BamComponent::INVALID;
        let res = unsafe { host_register_component(load_ctx.as_ffi_ptr(), &desc, &mut out_comp) };
        assert_eq!(res, FfiResult::OK);
        assert!(out_comp.is_valid());
        let updated_reg = load_ctx.registry.clone();
        load_ctx.finish_init();
        let cid = updated_reg.get_by_handle(out_comp).unwrap().component_id;
        app.world_mut().insert_resource(updated_reg);
        cid
    };

    let e_nonpod_active = {
        let e = app.world_mut().spawn_empty().id();
        let dummy: u64 = 0xCAFE_BABE;
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(dummy, |ptr| unsafe {
            emut.insert_by_id(non_pod_comp_id, ptr);
        });
        e
    };
    let e_nonpod_post_unload = {
        let e = app.world_mut().spawn_empty().id();
        let dummy: u64 = 0xFEED_FACE;
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(dummy, |ptr| unsafe {
            emut.insert_by_id(non_pod_comp_id, ptr);
        });
        e
    };

    app.world_mut().despawn(e_nonpod_active);
    assert_eq!(
        DROP_INVOCATION_COUNT.load(Ordering::SeqCst),
        1,
        "Active foreign drop function must execute upon entity despawn"
    );

    let mut rust_only_entities = Vec::new();
    let mut c_only_entities = Vec::new();
    let mut shared_entities = Vec::new();

    for i in 0..100 {
        let e = app.world_mut().spawn_empty().id();
        let r_init = RustModCustomData {
            power: 100.0 + (i as f32),
            shield: 10,
        };
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(r_init, |ptr| unsafe {
            emut.insert_by_id(rust_comp_id, ptr);
        });
        rust_only_entities.push(e);
    }

    for i in 0..100 {
        let e = app.world_mut().spawn_empty().id();
        let c_init = CModCustomData {
            mana: 50.0 + (i as f32),
            level: 1,
        };
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(c_init, |ptr| unsafe {
            emut.insert_by_id(c_comp_id, ptr);
        });
        c_only_entities.push(e);
    }

    for i in 0..100 {
        let e = app.world_mut().spawn_empty().id();
        let r_init = RustModCustomData {
            power: 200.0 + (i as f32),
            shield: 20,
        };
        let c_init = CModCustomData {
            mana: 100.0 + (i as f32),
            level: 5,
        };
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(r_init, |ptr| unsafe {
            emut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(c_init, |ptr| unsafe {
            emut.insert_by_id(c_comp_id, ptr);
        });
        shared_entities.push(e);
    }

    for _ in 0..5 {
        app.update();
    }

    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data.power, 150.0);
        assert_eq!(data.shield, 15);

        let ent = app.world().entity(c_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data.mana, 75.0);
        assert_eq!(data.level, 6);

        let ent = app.world().entity(shared_entities[0]);
        let r_data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let c_data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(r_data.power, 250.0);
        assert_eq!(r_data.shield, 25);
        assert_eq!(c_data.mana, 125.0);
        assert_eq!(c_data.level, 10);
    }

    // Asymmetric unload (unload Rust mod only)
    app.unload_mod(rust_mod_id_1).expect("Unload Rust mod");

    for _ in 0..5 {
        app.update();
    }

    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data.power, 150.0);
        assert_eq!(data.shield, 15);

        let ent = app.world().entity(c_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data.mana, 100.0);
        assert_eq!(data.level, 11);

        let ent = app.world().entity(shared_entities[0]);
        let r_data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let c_data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(r_data.power, 250.0);
        assert_eq!(r_data.shield, 25);
        assert_eq!(c_data.mana, 150.0);
        assert_eq!(c_data.level, 15);
    }

    // Unload C mod
    app.unload_mod(c_mod_id_1).expect("Unload C mod");

    for _ in 0..5 {
        app.update();
    }

    {
        let ent = app.world().entity(c_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data.mana, 100.0);
        assert_eq!(data.level, 11);

        let ent = app.world().entity(shared_entities[0]);
        let r_data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let c_data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(r_data.power, 250.0);
        assert_eq!(c_data.mana, 150.0);
    }

    // Post-unload despawn safety
    for e in rust_only_entities.drain(50..) {
        app.world_mut().despawn(e);
    }
    for e in c_only_entities.drain(50..) {
        app.world_mut().despawn(e);
    }
    for e in shared_entities.drain(50..) {
        app.world_mut().despawn(e);
    }

    non_pod_is_active.store(false, Ordering::Release);
    app.world_mut().despawn(e_nonpod_post_unload);
    assert_eq!(
        DROP_INVOCATION_COUNT.load(Ordering::SeqCst),
        1,
        "Foreign drop hook must NOT be invoked after mod unload (averting unmapped execution)"
    );

    // Reload both mods
    let rust_mod_id_2 = app
        .load_mod_with(&rust_dylib, shadow_load_options())
        .expect("Reload Rust mod");
    let c_mod_id_2 = app
        .load_mod_with(&c_dylib, shadow_load_options())
        .expect("Reload C mod");

    {
        let reg = app.world().resource::<ModRegistry>();
        assert_eq!(
            reg.get_by_name(RUST_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
            rust_comp_id
        );
        assert_eq!(
            reg.get_by_name(C_MOD_CUSTOM_DATA_NAME)
                .unwrap()
                .component_id,
            c_comp_id
        );
    }

    let mut new_entities = Vec::with_capacity(50);
    for i in 0..50 {
        let e = app.world_mut().spawn_empty().id();
        let r_init = RustModCustomData {
            power: 500.0 + (i as f32),
            shield: 50,
        };
        let c_init = CModCustomData {
            mana: 200.0 + (i as f32),
            level: 20,
        };
        let mut emut = app.world_mut().entity_mut(e);
        OwningPtr::make(r_init, |ptr| unsafe {
            emut.insert_by_id(rust_comp_id, ptr);
        });
        OwningPtr::make(c_init, |ptr| unsafe {
            emut.insert_by_id(c_comp_id, ptr);
        });
        new_entities.push(e);
    }

    // Resume simulation across both surviving entities and new entities
    for _ in 0..5 {
        app.update();
    }

    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data.power, 200.0);
        assert_eq!(data.shield, 20);

        let ent = app.world().entity(c_only_entities[0]);
        let data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data.mana, 125.0);
        assert_eq!(data.level, 16);

        let ent = app.world().entity(shared_entities[0]);
        let r_data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let c_data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(r_data.power, 300.0);
        assert_eq!(r_data.shield, 30);
        assert_eq!(c_data.mana, 175.0);
        assert_eq!(c_data.level, 20);
    }

    {
        let ent = app.world().entity(new_entities[0]);
        let r_data =
            unsafe { *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let c_data =
            unsafe { *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(r_data.power, 550.0);
        assert_eq!(r_data.shield, 55);
        assert_eq!(c_data.mana, 225.0);
        assert_eq!(c_data.level, 25);
    }

    app.unload_mod(rust_mod_id_2)
        .expect("Unload reloaded Rust mod");
    app.unload_mod(c_mod_id_2).expect("Unload reloaded C mod");

    for e in rust_only_entities
        .into_iter()
        .chain(c_only_entities)
        .chain(shared_entities)
        .chain(new_entities)
    {
        app.world_mut().despawn(e);
    }
}

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq)]
#[mod_component(name = "e2e_layout::ColorRgbaLayout")]
pub struct ColorRgbaLayout {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

#[test]
fn test_end_to_end_asymmetric_rust_and_c99_optional_changed_resources_and_commands() {
    let (shared_rust_dylib, shared_c_dylib) = ensure_guest_mods_built();
    let rust_dylib = create_shadow_copy(&shared_rust_dylib, "cond_isolated_rust");
    let c_dylib = create_shadow_copy(&shared_c_dylib, "cond_isolated_c");

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<NameTag>(ComponentOptions::new().with_default())
        .unwrap();

    app.register_mod_component::<FilteredTransform>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<ChangeSensor>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<FreshSpawnTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<TransientMarker>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_resource::<RustFilterTelemetry>().unwrap();
    app.insert_resource(RustFilterTelemetry {
        scale: 2.0,
        matched_rows: 0,
        optional_vel_hits: 0,
        spawned_default_entity: 0,
    });

    app.register_mod_component::<CFilteredTransform>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<CChangeSensor>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<CFreshSpawnTag>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_component::<CTransientMarker>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .unwrap();
    app.register_mod_resource::<CFilterTelemetry>().unwrap();
    app.insert_resource(CFilterTelemetry {
        scale: 3.0,
        matched_rows: 0,
        optional_vel_hits: 0,
        spawned_default_entity: 0,
    });

    let strict_opts = LoadOptions {
        strict: true,
        shadow_copy: false,
        ..Default::default()
    };
    let rust_mod_id = app
        .load_mod_with(&rust_dylib, strict_opts.clone())
        .expect("Strict load of asymmetric_rust_mod must succeed");
    let c_mod_id = app
        .load_mod_with(&c_dylib, strict_opts)
        .expect("Strict load of asymmetric_c_mod must succeed");

    // SAFETY: Loading our isolated per-test shadow copies to toggle run_if flags.
    let rust_lib = unsafe { Library::new(&rust_dylib).unwrap() };
    let c_lib = unsafe { Library::new(&c_dylib).unwrap() };
    let set_rust_enabled: Symbol<unsafe extern "C" fn(FfiBool)> = unsafe {
        rust_lib
            .get(b"asymmetric_rust_mod_set_conditional_enabled\0")
            .unwrap()
    };
    let set_c_enabled: Symbol<unsafe extern "C" fn(FfiBool)> = unsafe {
        c_lib
            .get(b"asymmetric_c_mod_set_conditional_enabled\0")
            .unwrap()
    };
    unsafe {
        set_rust_enabled(FfiBool::TRUE);
        set_c_enabled(FfiBool::TRUE);
    }

    let r_with_vel = app
        .world_mut()
        .spawn((
            FilteredTransform {
                x: 1.0,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 10.0,
                y: 0.0,
                z: 0.0,
            },
            ChangeSensor { val: 1 },
            FreshSpawnTag { code: 1 },
            TransientMarker { dummy: 9 },
        ))
        .id();
    let r_no_vel = app
        .world_mut()
        .spawn((
            FilteredTransform {
                x: 100.0,
                y: 0.0,
                z: 0.0,
            },
            ChangeSensor { val: 2 },
            FreshSpawnTag { code: 1 },
            TransientMarker { dummy: 9 },
        ))
        .id();

    let c_with_vel = app
        .world_mut()
        .spawn((
            CFilteredTransform {
                x: 0.0,
                y: 2.0,
                z: 0.0,
            },
            Velocity {
                x: 0.0,
                y: 5.0,
                z: 0.0,
            },
            CChangeSensor { val: 1 },
            CFreshSpawnTag { code: 1 },
            CTransientMarker { dummy: 9 },
        ))
        .id();
    let c_no_vel = app
        .world_mut()
        .spawn((
            CFilteredTransform {
                x: 0.0,
                y: 200.0,
                z: 0.0,
            },
            CChangeSensor { val: 2 },
            CFreshSpawnTag { code: 1 },
            CTransientMarker { dummy: 9 },
        ))
        .id();

    app.update();

    {
        let w = app.world();
        assert_eq!(w.get::<FilteredTransform>(r_with_vel).unwrap().x, 21.0);
        assert_eq!(w.get::<FilteredTransform>(r_no_vel).unwrap().x, 102.0);
        assert!(w.get::<TransientMarker>(r_with_vel).is_none());
        assert!(w.get::<TransientMarker>(r_no_vel).is_none());

        let r_res = w.resource::<RustFilterTelemetry>();
        assert_eq!(r_res.matched_rows, 2);
        assert_eq!(r_res.optional_vel_hits, 1);
        assert_ne!(r_res.spawned_default_entity, 0);
        let r_spawned = Entity::from_bits(r_res.spawned_default_entity);
        assert!(w.get::<NameTag>(r_spawned).is_some());

        assert_eq!(w.get::<CFilteredTransform>(c_with_vel).unwrap().y, 17.0);
        assert_eq!(w.get::<CFilteredTransform>(c_no_vel).unwrap().y, 203.0);
        assert!(w.get::<CTransientMarker>(c_with_vel).is_none());
        assert!(w.get::<CTransientMarker>(c_no_vel).is_none());

        let c_res = w.resource::<CFilterTelemetry>();
        assert_eq!(c_res.matched_rows, 2);
        assert_eq!(c_res.optional_vel_hits, 1);
        assert_ne!(c_res.spawned_default_entity, 0);
        let c_spawned = Entity::from_bits(c_res.spawned_default_entity);
        assert!(w.get::<NameTag>(c_spawned).is_some());
    }

    app.world_mut()
        .get_mut::<ChangeSensor>(r_with_vel)
        .unwrap()
        .val = 77;
    app.world_mut()
        .get_mut::<CChangeSensor>(c_with_vel)
        .unwrap()
        .val = 77;
    app.update();
    assert_eq!(
        app.world().resource::<RustFilterTelemetry>().matched_rows,
        2
    );
    assert_eq!(app.world().resource::<CFilterTelemetry>().matched_rows, 2);

    unsafe {
        set_rust_enabled(FfiBool::FALSE);
        set_c_enabled(FfiBool::FALSE);
    }
    let r_delayed = app
        .world_mut()
        .spawn((
            FilteredTransform {
                x: 50.0,
                y: 0.0,
                z: 0.0,
            },
            ChangeSensor { val: 5 },
            FreshSpawnTag { code: 5 },
        ))
        .id();
    let c_delayed = app
        .world_mut()
        .spawn((
            CFilteredTransform {
                x: 0.0,
                y: 60.0,
                z: 0.0,
            },
            CChangeSensor { val: 5 },
            CFreshSpawnTag { code: 5 },
        ))
        .id();

    app.update();
    assert_eq!(
        app.world().resource::<RustFilterTelemetry>().matched_rows,
        2
    );
    assert_eq!(app.world().resource::<CFilterTelemetry>().matched_rows, 2);
    assert_eq!(
        app.world().get::<FilteredTransform>(r_delayed).unwrap().x,
        50.0
    );
    assert_eq!(
        app.world().get::<CFilteredTransform>(c_delayed).unwrap().y,
        60.0
    );

    unsafe {
        set_rust_enabled(FfiBool::TRUE);
        set_c_enabled(FfiBool::TRUE);
    }
    app.update();
    assert_eq!(
        app.world().resource::<RustFilterTelemetry>().matched_rows,
        3
    );
    assert_eq!(app.world().resource::<CFilterTelemetry>().matched_rows, 3);
    assert_eq!(
        app.world().get::<FilteredTransform>(r_delayed).unwrap().x,
        52.0
    );
    assert_eq!(
        app.world().get::<CFilteredTransform>(c_delayed).unwrap().y,
        63.0
    );

    drop(rust_lib);
    drop(c_lib);
    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
    remove_shadow_copy_and_parent(&rust_dylib);
    remove_shadow_copy_and_parent(&c_dylib);
}

#[test]
fn test_cross_language_interop_binary_layout() {
    assert_eq!(core::mem::size_of::<Position>(), 12);
    assert_eq!(core::mem::align_of::<Position>(), 4);
    let pos_sample = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let pos_base = &pos_sample as *const _ as usize;
    assert_eq!((&pos_sample.x as *const _ as usize) - pos_base, 0);
    assert_eq!((&pos_sample.y as *const _ as usize) - pos_base, 4);
    assert_eq!((&pos_sample.z as *const _ as usize) - pos_base, 8);

    assert_eq!(core::mem::size_of::<Velocity>(), 12);
    assert_eq!(core::mem::align_of::<Velocity>(), 4);

    assert_eq!(core::mem::size_of::<CombatHealth>(), 8);
    assert_eq!(core::mem::align_of::<CombatHealth>(), 4);
    let health_sample = CombatHealth {
        current: 10,
        max: 20,
    };
    let health_base = &health_sample as *const _ as usize;
    assert_eq!(
        (&health_sample.current as *const _ as usize) - health_base,
        0
    );
    assert_eq!((&health_sample.max as *const _ as usize) - health_base, 4);

    assert_eq!(core::mem::size_of::<ColorRgbaLayout>(), 16);
    assert_eq!(core::mem::align_of::<ColorRgbaLayout>(), 4);
    let color_sample = ColorRgbaLayout {
        r: 0.1,
        g: 0.2,
        b: 0.3,
        a: 1.0,
    };
    let color_base = &color_sample as *const _ as usize;
    assert_eq!((&color_sample.r as *const _ as usize) - color_base, 0);
    assert_eq!((&color_sample.g as *const _ as usize) - color_base, 4);
    assert_eq!((&color_sample.b as *const _ as usize) - color_base, 8);
    assert_eq!((&color_sample.a as *const _ as usize) - color_base, 12);

    let raw_c_bytes: [u8; 12] = [
        0x00, 0x00, 0x80, 0x3F, // 1.0f32
        0x00, 0x00, 0x00, 0x40, // 2.0f32
        0x00, 0x00, 0x40, 0x40, // 3.0f32
    ];
    // SAFETY: `raw_c_bytes` contains 12 initialized IEEE-754 little-endian bytes for `Position`.
    let pos_unmarshaled: Position =
        unsafe { core::ptr::read_unaligned(raw_c_bytes.as_ptr() as *const Position) };
    assert_eq!(
        pos_unmarshaled,
        Position {
            x: 1.0,
            y: 2.0,
            z: 3.0
        }
    );
}
