#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use abi_contract::{
    ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter, RawCommandQueue,
};
use bevy_app::{App, PostUpdate, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{ResMut, Resource, World};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::{LogLevel, MultiThreadedExecutor, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{Query, System};
use bevy_ecs::world::CommandQueue;
use common::*;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::commands::{host_cmd_despawn, CommandQueueBridge};
use host_runtime::dynamic_system::{host_register_component, DynamicCSystem, ModInitContext};
use host_runtime::{
    host_cmd_insert_component, host_cmd_spawn_empty, AbiModPlugin, ModAppExt, ModHost, ModRegistry,
};
use libloading::{Library, Symbol};
use std::sync::Arc;

// ============================================================================
// Milestone 3: Bevy Runtime Testbed Integration Test Suite
// ============================================================================

/// Scenario 1: Validation Gate 3 (Original baseline: 100 ticks multi-threaded schedule,
/// guest panic containment, hot-unloading, and 10 post-unload ticks).
#[test]
fn test_validation_gate_3_asymmetric_mods_concurrency_fault_isolation_and_lifecycle() {
    let (shared_rust_dylib, shared_c_dylib) = ensure_guest_mods_built();
    let pid = std::process::id();
    let ext = shared_rust_dylib
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("dylib");
    let rust_dylib = std::env::temp_dir().join(format!("vg3_isolated_rust_{pid}.{ext}"));
    let c_dylib = std::env::temp_dir().join(format!("vg3_isolated_c_{pid}.{ext}"));
    let _ = std::fs::remove_file(&rust_dylib);
    let _ = std::fs::remove_file(&c_dylib);
    std::fs::copy(&shared_rust_dylib, &rust_dylib).expect("copy isolated rust dylib");
    std::fs::copy(&shared_c_dylib, &c_dylib).expect("copy isolated c dylib");

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

    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
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

    // Trigger intentional panic in asymmetric_rust_mod
    unsafe {
        let lib = Library::new(&rust_dylib).expect("Failed to open rust_dylib for panic trigger");
        let trigger: Symbol<unsafe extern "C" fn()> = lib
            .get(b"asymmetric_rust_mod_trigger_panic\0")
            .expect("Failed to resolve asymmetric_rust_mod_trigger_panic");
        trigger();
    }

    // Tick with panic triggered: isolated via PANIC_CAUGHT
    app.update();
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

    // Clean up panic state and temporary isolated dylibs
    reset_rust_mod_panic(&rust_dylib);
    let _ = std::fs::remove_file(&rust_dylib);
    let _ = std::fs::remove_file(&c_dylib);
}

/// Scenario 2: Multi-Archetype Chunk Iteration with C and Rust Guest Mods Operating Simultaneously.
/// Exercises 10,000 entities partitioned across 4 distinct archetypes.
#[test]
fn test_multi_archetype_chunk_iteration_simultaneous_c_and_rust() {
    let (rust_dylib, _c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut builder = TestbedBuilder::new();

    // Spawn 10,000 entities across 4 archetypes (2,500 each):
    // Arch 0: (Position, Velocity)
    // Arch 1: (Position, Velocity, NameTag)
    // Arch 2: (Position, CombatHealth)
    // Arch 3: (Position, Mass)
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

    let (mut testbed, r_id, c_id) = builder.build_and_load_mods();

    // Execute 20 ticks
    testbed.tick(20);

    // Verify kinematics updates across archetypes:
    // Rust mod queries (Position WRITE, Velocity READ) -> applies to Arch 0 and Arch 1 (5,000 entities):
    //   pos.y += 20 * 2.0 = 40.0
    // C mod queries (Position WRITE) -> applies to ALL 4 Archetypes (10,000 entities):
    //   pos.z += 20 * 1.0 = 20.0

    // Query Arch 0 & 1 (with Velocity)
    let mut count_vel = 0;
    let mut vel_query = testbed.world_mut().query::<(&Position, &Velocity)>();
    for (pos, _vel) in vel_query.iter(testbed.world()) {
        assert_eq!(pos.y, 40.0, "pos.y must equal 40.0 from rust kinematics");
        assert_eq!(pos.z, 20.0, "pos.z must equal 20.0 from c kinematics");
        count_vel += 1;
    }
    assert_eq!(count_vel, 2 * PER_ARCH);

    // Query Arch 3 (with Mass, no Velocity): pos.y must be 0.0, pos.z must be 20.0
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

    // Query Arch 2 (CombatHealth): health decremented by combat systems
    // Rust combat: -15 per tick = -300
    // C combat: -10 per tick = -200
    // Total reduction = -500 -> health reaches 0, which triggers despawn!
    let mut health_query = testbed.world_mut().query::<(&Position, &CombatHealth)>();
    let remaining_health_entities = health_query.iter(testbed.world()).count();
    // Since health reached 0 on tick 20, they were despawned deferred!
    assert_eq!(
        remaining_health_entities, 0,
        "Combat entities reaching 0 HP must have been despawned via CommandQueueBridge"
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
/// Confirms that chunk iteration component mutations properly trigger Bevy's native change detection.
#[test]
fn test_change_detection_position_and_combat_health() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangeDetectionTracker>();
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<Mass>();

    // Entity 1: possesses Position + Velocity (mutated by kinematics systems)
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

    // Entity 2: possesses CombatHealth (mutated by combat systems)
    let e2 = app
        .world_mut()
        .spawn(CombatHealth {
            current: 1000,
            max: 1000,
        })
        .id();

    // Entity 3: possesses Mass only (never mutated by any mod system)
    let e3 = app.world_mut().spawn(Mass { value: 42.0 }).id();

    // Add tracker system to observe change events across frames
    app.add_systems(Update, track_changes_system);

    // Initial tick to flush initial entity spawn changes
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

    // Frame 2: Blank update before mods loaded - zero changes observed!
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

    // Now load mods
    let r_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load rust mod");
    let c_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load c mod");

    // Execute 1 tick with mods active: kinematics mutates e1, combat mutates e2
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
/// Validates string reads/writes across C-ABI without allocator boundary violations.
#[test]
fn test_dynamic_metadata_string_strview_inspection() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<MetadataTag>();

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

    // Load C mod: c_metadata_inspector checks for "grunt" and upgrades to "elite_grunt"
    let c_id = app
        .load_mod_tracked(&c_dylib)
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

    // Now load Rust mod: rust_metadata prepends "Modded_" to labels not starting with "Modded_"
    let r_id = app
        .load_mod_tracked(&rust_dylib)
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
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut core::ffi::c_void,
) -> abi_contract::FfiResult {
    if commands.is_null() {
        return abi_contract::FfiResult::NULL_POINTER;
    }
    // SAFETY: caller provided valid RawCommandQueue; calling C-ABI command bridge.
    unsafe {
        let spawned = host_cmd_spawn_empty(commands);
        let p = Position {
            x: 77.0,
            y: 88.0,
            z: 99.0,
        };
        host_cmd_insert_component(
            commands,
            spawned,
            c"Position".as_ptr(),
            &p as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        );
    }
    abi_contract::FfiResult::OK
}

/// Scenario 5: Deferred Commands (`spawn_empty`, `insert_component`, `despawn`) Flush Verification.
/// Confirms that guest system deferred commands queue and execute cleanly in Bevy's schedule.
#[test]
fn test_deferred_commands_spawn_insert_despawn_flush() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<CombatHealth>();

    // Spawn an entity with low health (10 HP)
    // When C combat damage system (-10 HP) executes, health drops to 0, enqueuing despawn!
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

    // Register a dynamic C system that enqueues spawn_empty + insert_component Position { 77.0, 88.0, 99.0 }
    let registry = app.world().resource::<ModRegistry>().clone();
    let cmd_system = DynamicCSystem::new(
        "custom_command_spawner".to_string(),
        vec![],
        custom_command_spawner_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );
    app.add_systems(Update, cmd_system);

    // Load C mod which has c_combat_damage
    let c_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load c mod");

    // Execute 1 tick:
    // 1. c_combat_damage damages dying_entity to 0 -> enqueues despawn.
    // 2. custom_command_spawner enqueues spawn_empty + insert_component Position { 77.0, 88.0, 99.0 }.
    // 3. System::apply_deferred flushes CommandQueueBridge to Bevy World!
    app.update();

    // Verify dying entity is despawned
    assert!(
        app.world().get_entity(dying_entity).is_err(),
        "dying_entity must be despawned from the Bevy World after apply_deferred"
    );

    // Verify newly spawned entity with Position { 77.0, 88.0, 99.0 } exists
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

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct RustModCustomData {
    pub power: f32,
    pub shield: u32,
}

#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq)]
struct CModCustomData {
    pub mana: f32,
    pub level: i32,
}

/// Scenario 6: Milestone 3 Multi-Language Dynamic Component Lifecycle & Concurrency Test.
///
/// Verifies:
/// 1. Host loads both `asymmetric_rust_mod` and `asymmetric_c_mod`.
/// 2. Host verifies both `RustModCustomData` and `CModCustomData` are dynamically registered in `ModRegistry`.
/// 3. Host spawns entities possessing dynamic components (RustModCustomData, CModCustomData, and both on a shared entity).
/// 4. `app.update()` runs:
///    - Rust system mutates `power += 10.0`, `shield += 1` on matching entities.
///    - C99 system mutates `mana += 5.0f`, `level += 1` on matching entities.
///    - Single entity with both components receives mutations from both foreign systems in the same tick.
/// 5. Change detection: entity component change ticks reflect the mutation tick.
/// 6. Mod unloading: unloading mods cleanly deactivates foreign systems, leaves component table data intact,
///    prevents further mutations on subsequent ticks, and despawning post-unload produces zero segfaults.
#[test]
fn test_milestone_3_dynamic_components_multilanguage() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // 1. Load both mods
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    // 2. Verify ModRegistry dynamic component metadata
    let (rust_comp_id, c_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();

        let rust_meta = registry
            .get_by_name("RustModCustomData")
            .expect("RustModCustomData must be registered in ModRegistry");
        assert_eq!(rust_meta.size, 8);
        assert_eq!(rust_meta.align, 4);
        assert_eq!(
            rust_meta.uuid,
            ComponentUuid::from_name("RustModCustomData")
        );
        assert!(rust_meta.is_dynamic);

        let c_meta = registry
            .get_by_name("CModCustomData")
            .expect("CModCustomData must be registered in ModRegistry");
        assert_eq!(c_meta.size, 8);
        assert_eq!(c_meta.align, 4);
        assert_eq!(c_meta.uuid, ComponentUuid::from_name("CModCustomData"));
        assert!(c_meta.is_dynamic);

        (rust_meta.component_id, c_meta.component_id)
    };

    // 3. Spawn entities with dynamic components
    // Entity 1: Rust dynamic component only
    let e_rust = {
        let e = app.world_mut().spawn_empty().id();
        let init_data = RustModCustomData {
            power: 100.0,
            shield: 50,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| {
            // SAFETY: RustModCustomData layout matches registered descriptor.
            unsafe {
                entity_mut.insert_by_id(rust_comp_id, ptr);
            }
        });
        e
    };

    // Entity 2: C dynamic component only
    let e_c = {
        let e = app.world_mut().spawn_empty().id();
        let init_data = CModCustomData {
            mana: 25.0,
            level: 3,
        };
        let mut entity_mut = app.world_mut().entity_mut(e);
        OwningPtr::make(init_data, |ptr| {
            // SAFETY: CModCustomData layout matches registered descriptor.
            unsafe {
                entity_mut.insert_by_id(c_comp_id, ptr);
            }
        });
        e
    };

    // Entity 3: BOTH dynamic components on a shared entity (multi-language heterogeneous archetype)
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
        OwningPtr::make(init_rust, |ptr| {
            // SAFETY: RustModCustomData layout matches registered descriptor.
            unsafe {
                entity_mut.insert_by_id(rust_comp_id, ptr);
            }
        });
        OwningPtr::make(init_c, |ptr| {
            // SAFETY: CModCustomData layout matches registered descriptor.
            unsafe {
                entity_mut.insert_by_id(c_comp_id, ptr);
            }
        });
        e
    };

    // Record initial change ticks prior to system execution
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

    // 4. Execute 1 tick of the Bevy schedule
    app.update();

    let frame1_tick = app.world_mut().change_tick();

    // Verify mutations on Entity 1 (RustModCustomData: power +10.0, shield +1)
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
            },
            "Rust mod must increment power by 10.0 and shield by 1"
        );
        let ticks = entity_ref.get_change_ticks_by_id(rust_comp_id).unwrap();
        assert_ne!(ticks.changed, initial_rust_tick);
        assert!(ticks.is_changed(initial_rust_tick, frame1_tick));
    }

    // Verify mutations on Entity 2 (CModCustomData: mana +5.0f, level +1)
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
            },
            "C99 mod must increment mana by 5.0 and level by 1"
        );
        let ticks = entity_ref.get_change_ticks_by_id(c_comp_id).unwrap();
        assert_ne!(ticks.changed, initial_c_tick);
        assert!(ticks.is_changed(initial_c_tick, frame1_tick));
    }

    // Verify mutations on Entity 3 (Shared archetype mutated by BOTH Rust and C99 systems)
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
            },
            "Shared entity Rust component must be mutated by Rust mod"
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
            },
            "Shared entity C component must be mutated by C99 mod"
        );

        let ticks_rust = entity_ref.get_change_ticks_by_id(rust_comp_id).unwrap();
        let ticks_c = entity_ref.get_change_ticks_by_id(c_comp_id).unwrap();
        assert_ne!(
            ticks_rust.changed, ticks_c.changed,
            "Distinct dynamic systems must each advance Bevy's change tick"
        );
        assert_ne!(ticks_rust.changed, initial_both_rust_tick);
        assert_ne!(ticks_c.changed, initial_both_c_tick);
        assert!(ticks_rust.is_changed(initial_both_rust_tick, frame1_tick));
        assert!(ticks_c.is_changed(initial_both_c_tick, frame1_tick));
    }

    // Execute 4 more ticks: total 5 ticks
    for _ in 0..4 {
        app.update();
    }

    // Verify values after 5 ticks total
    // Rust: +10.0 power per tick * 5 = +50.0; +1 shield * 5 = +5
    // C: +5.0 mana per tick * 5 = +25.0; +1 level * 5 = +5
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

    // 5. Mod unloading & lifecycle safety
    app.unload_mod(rust_mod_id)
        .expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id).expect("C mod must unload cleanly");

    // Post-unload ticks: components remain intact, values frozen (no further system execution)
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
            },
            "Values must remain frozen after mod unload"
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
            },
            "Values must remain frozen after mod unload"
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

    // Despawn all entities post-unload without panics or crashes
    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
    app.world_mut().despawn(e_both);
}

// ============================================================================
// Milestone 4: End-to-End Integration Test Suite
// ============================================================================

/// Tracks native change detection in downstream Bevy systems across ticks.
#[derive(Resource, Default)]
struct DownstreamNativeTracker {
    changed_position_entities: Vec<Entity>,
    tick_count: usize,
}

/// Downstream native Bevy system observing `Changed<Position>` in PostUpdate schedule.
fn downstream_native_position_observer(
    query: Query<Entity, Changed<Position>>,
    mut tracker: ResMut<DownstreamNativeTracker>,
) {
    tracker.changed_position_entities = query.iter().collect();
    tracker.tick_count += 1;
}

/// Test 1 (Milestone 4.1): High-Throughput Archetype Permutations & Change Detection
///
/// Verifies:
/// 1. Host registers static `Position` and `Velocity` components first.
/// 2. Host loads both `libasymmetric_rust_mod.dylib` and `libasymmetric_c_mod.dylib`.
/// 3. Spawns 10,000+ entities across 4 archetype permutations (2,500 each):
///    - Permutation 1: [RustModCustomData] (mutated by rust_custom_system)
///    - Permutation 2: [CModCustomData] (mutated by c_custom_data_system)
///    - Permutation 3: [RustModCustomData, CModCustomData] (mutated by both systems)
///    - Permutation 4: [Position, RustModCustomData, CModCustomData] (mutated by c_kinematics + both custom systems)
/// 4. Runs 10-tick simulation.
/// 5. Verifies zero-copy chunk column mutation across both languages.
/// 6. Verifies Bevy ECS change detection:
///    - Downstream native system in `PostUpdate` detects `Changed<Position>` on all Permutation 4 entities.
///    - Entity component change ticks reflect the simulation tick advances.
/// 7. Verifies clean mod unloading, value freezing across 5 post-unload frames, and clean post-unload entity despawns.
#[test]
fn test_milestone_4_high_throughput_archetype_permutations() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // 1. Register host static exposable components required by asymmetric mods
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // 2. Add downstream change detection observer in PostUpdate schedule
    app.init_resource::<DownstreamNativeTracker>();
    app.add_systems(PostUpdate, downstream_native_position_observer);

    // 3. Load both asymmetric mods
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    // 4. Resolve component IDs from ModRegistry
    let (rust_comp_id, c_comp_id, pos_comp_id) = {
        let registry = app.world().resource::<ModRegistry>();
        let r_meta = registry
            .get_by_name("RustModCustomData")
            .expect("RustModCustomData must be registered");
        let c_meta = registry
            .get_by_name("CModCustomData")
            .expect("CModCustomData must be registered");
        let p_meta = registry
            .get_by_name("Position")
            .expect("Position must be registered");

        assert!(r_meta.is_dynamic);
        assert!(c_meta.is_dynamic);
        assert_eq!(r_meta.size, 8);
        assert_eq!(c_meta.size, 8);
        (r_meta.component_id, c_meta.component_id, p_meta.component_id)
    };

    const PER_ARCHETYPE: usize = 2_500;
    const TOTAL_ENTITIES: usize = PER_ARCHETYPE * 4;
    let pre_spawn_count = app.world_mut().query::<Entity>().iter(app.world()).count();

    // 5. Spawn 10,000 entities across 4 permutations
    let mut arch1_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch2_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch3_entities = Vec::with_capacity(PER_ARCHETYPE);
    let mut arch4_entities = Vec::with_capacity(PER_ARCHETYPE);

    // Permutation 1: [RustModCustomData] (Rust dynamic only)
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

    // Permutation 2: [CModCustomData] (C99 dynamic only)
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

    // Permutation 3: [RustModCustomData, CModCustomData] (Shared dynamic archetype)
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

    // Permutation 4: [Position, RustModCustomData, CModCustomData] (Static Position + dual dynamic)
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

    // 6. Execute 10 simulation ticks
    const SIM_TICKS: usize = 10;
    for _ in 1..=SIM_TICKS {
        app.update();

        // Verify downstream native change detection for Position in PostUpdate:
        // Every tick, all 2,500 Permutation 4 entities must trigger Changed<Position>
        let tracker = app.world().resource::<DownstreamNativeTracker>();
        assert!(
            tracker.changed_position_entities.len() >= PER_ARCHETYPE,
            "At least {PER_ARCHETYPE} Permutation 4 entities must trigger Changed<Position> in PostUpdate"
        );
    }

    let post_sim_tick = app.world_mut().change_tick();

    // 7. Verify zero-copy column mutations across all 10,000 entities
    // Check Permutation 1 (Rust only): power +10/tick, shield +1/tick
    for (i, &e) in arch1_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data.power, 100.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data.shield, 10 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert!(ent.get_by_id(c_comp_id).is_err());
        assert!(ent.get_by_id(pos_comp_id).is_err());

        let ticks = ent.get_change_ticks_by_id(rust_comp_id).unwrap();
        assert!(ticks.is_changed(pre_sim_tick, post_sim_tick));
    }

    // Check Permutation 2 (C only): mana +5/tick, level +1/tick
    for (i, &e) in arch2_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 50.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data.level, 1 + ((i % 20) as i32) + (SIM_TICKS as i32));
        assert!(ent.get_by_id(rust_comp_id).is_err());
        assert!(ent.get_by_id(pos_comp_id).is_err());

        let ticks = ent.get_change_ticks_by_id(c_comp_id).unwrap();
        assert!(ticks.is_changed(pre_sim_tick, post_sim_tick));
    }

    // Check Permutation 3 (Both dynamic): mutated by both foreign systems
    for (i, &e) in arch3_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let data_r = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 200.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data_r.shield, 20 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert_eq!(data_c.mana, 100.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data_c.level, 5 + ((i % 20) as i32) + (SIM_TICKS as i32));
        assert!(ent.get_by_id(pos_comp_id).is_err());
    }

    // Check Permutation 4 (Static Position + Both dynamic): mutated by c_kinematics + both dynamic systems
    for (i, &e) in arch4_entities.iter().enumerate() {
        let ent = app.world().entity(e);
        let pos = ent.get::<Position>().unwrap();
        assert_eq!(pos.x, i as f32);
        assert_eq!(pos.y, 0.0);
        assert_eq!(pos.z, SIM_TICKS as f32 * 1.0);

        let data_r = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let data_c = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data_r.power, 300.0 + (i as f32) + (SIM_TICKS as f32 * 10.0));
        assert_eq!(data_r.shield, 30 + ((i % 50) as u32) + (SIM_TICKS as u32));
        assert_eq!(data_c.mana, 150.0 + (i as f32) + (SIM_TICKS as f32 * 5.0));
        assert_eq!(data_c.level, 10 + ((i % 20) as i32) + (SIM_TICKS as i32));
    }

    // 8. Unload both mods cleanly
    app.unload_mod(rust_mod_id)
        .expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id)
        .expect("C mod must unload cleanly");

    // 9. Post-unload ticks: foreign systems deactivated, values frozen
    for _ in 0..5 {
        app.update();
    }

    let tracker_post = app.world().resource::<DownstreamNativeTracker>();
    assert_eq!(
        tracker_post.changed_position_entities.len(),
        0,
        "Post-unload ticks must produce 0 Changed<Position> events"
    );

    // Verify values remain strictly frozen at tick 10 values
    let sample_e4 = arch4_entities[0];
    let pos_frozen = app.world().entity(sample_e4).get::<Position>().unwrap();
    assert_eq!(pos_frozen.z, SIM_TICKS as f32 * 1.0);

    // 10. Despawn all 10,000 entities post-unload without panics or memory leaks
    for e in arch1_entities
        .into_iter()
        .chain(arch2_entities)
        .chain(arch3_entities)
        .chain(arch4_entities)
    {
        app.world_mut().despawn(e);
    }
    // Verify all original 10k entities despawned cleanly
    assert!(app.world().get_entity(sample_e4).is_err());
}

/// Test 2 (Milestone 4.2): Parallel Scheduler Concurrency under MultiThreadedExecutor
///
/// Verifies:
/// 1. Disjoint dynamic systems (`rust_custom_system` with WRITE `RustModCustomData` and
///    `c_custom_data_system` with WRITE `CModCustomData`) possess compatible access sets.
/// 2. Parallel Bevy schedule with `MultiThreadedExecutor::default()` and
///    `ScheduleBuildSettings { ambiguity_detection: LogLevel::Error, ..Default::default() }`
///    initializes and runs with zero borrow conflicts or ambiguity errors.
/// 3. Correct parallel state mutation under high entity counts (10,000 entities with both components).
/// 4. Empirical barrier synchronization proving concurrent multi-threaded execution.
#[test]
fn test_milestone_4_parallel_scheduler_concurrency() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // Load both mods and extract dynamic systems directly
    let (rust_mod_id, rust_systems) = app
        .world_mut()
        .resource_scope::<ModHost, _>(|world, mut mod_host| unsafe {
            mod_host.load_mod_tracked_with_world(&rust_dylib, world)
        })
        .expect("Failed to load asymmetric_rust_mod");

    let (c_mod_id, c_systems) = app
        .world_mut()
        .resource_scope::<ModHost, _>(|world, mut mod_host| unsafe {
            mod_host.load_mod_tracked_with_world(&c_dylib, world)
        })
        .expect("Failed to load asymmetric_c_mod");

    let rust_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("RustModCustomData")
        .unwrap()
        .component_id;
    let c_comp_id = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("CModCustomData")
        .unwrap()
        .component_id;

    // Extract disjoint systems by their declared component access:
    // rust_custom_system queries RustModCustomData (WRITE)
    // c_custom_data_system queries CModCustomData (WRITE)
    let mut sys_rust = None;
    for mut s in rust_systems {
        if let bevy_ecs::system::SystemAccess::Shared(set) = s.initialize(app.world_mut()) {
            if set.combined_access().has_write(rust_comp_id) {
                sys_rust = Some(s);
                break;
            }
        }
    }
    let mut sys_rust = sys_rust.expect("rust_custom_system querying RustModCustomData must exist");

    let mut sys_c = None;
    for mut s in c_systems {
        if let bevy_ecs::system::SystemAccess::Shared(set) = s.initialize(app.world_mut()) {
            if set.combined_access().has_write(c_comp_id) {
                sys_c = Some(s);
                break;
            }
        }
    }
    let mut sys_c = sys_c.expect("c_custom_data_system querying CModCustomData must exist");

    // Verify access compatibility between the two disjoint dynamic systems
    let access_rust = sys_rust.initialize(app.world_mut());
    let access_c = sys_c.initialize(app.world_mut());
    assert!(
        access_rust.is_compatible(&access_c),
        "Disjoint dynamic systems must have compatible access for parallel execution"
    );

    // Build parallel schedule with MultiThreadedExecutor and strict ambiguity detection
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

    // Spawn 10,000 entities possessing BOTH dynamic components
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

    // Run parallel schedule for 5 ticks
    const TICKS: usize = 5;
    for _ in 0..TICKS {
        schedule.run(app.world_mut());
    }

    // Verify all 10,000 entities mutated correctly without race conditions
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

    // Clean teardown
    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
}

/// Test 3 (Milestone 4.3): Deferred Command Queue Burst & Allocator Isolation
///
/// Verifies:
/// 1. High-volume burst enqueueing of dynamic component insertions for 1,000+ entities
///    via raw command queues (`host_cmd_spawn_empty`, `host_cmd_insert_component`).
/// 2. Strict Allocator Isolation (Rule 2) via memory poisoning: mutating guest memory buffers
///    post-enqueue before flush/apply does NOT affect the data stored in Bevy World storage,
///    proving host `AlignedBuffer` copy and zero guest pointer aliasing.
/// 3. Despawn-before-apply safety: queuing component insertion on an entity and immediately
///    despawning it before apply executes cleanly without panics, and safely drops host buffers.
/// 4. Flushed burst entities are seamlessly queryable and mutable by foreign systems.
#[test]
fn test_milestone_4_deferred_command_queue_burst_and_allocator_isolation() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load asymmetric_rust_mod");
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load asymmetric_c_mod");

    let world_reg = app.world().resource::<ModRegistry>().clone();
    let rust_comp_id = world_reg.get_by_name("RustModCustomData").unwrap().component_id;
    let c_comp_id = world_reg.get_by_name("CModCustomData").unwrap().component_id;

    let mut command_queue = CommandQueue::default();
    let mut bridge = CommandQueueBridge::new(
        &mut command_queue,
        app.world().entity_allocator(),
        &world_reg,
    );
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    // 1. Memory Poisoning Test (Rule 2: Allocator Isolation)
    // Spawn entity and enqueue component insertion using mutable local guest buffers
    let ffi_poison_rust = unsafe { host_cmd_spawn_empty(raw_queue) };
    let mut guest_rust_buf = RustModCustomData {
        power: 777.75,
        shield: 999,
    };
    let res_r = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_poison_rust,
            c"RustModCustomData".as_ptr(),
            &guest_rust_buf as *const _ as *const u8,
            core::mem::size_of::<RustModCustomData>(),
        )
    };
    assert_eq!(res_r, FfiResult::OK);

    // Immediately POISON guest buffer post-enqueue before command_queue.apply()!
    guest_rust_buf.power = -99999.0;
    guest_rust_buf.shield = 0xDEADBEEF;
    assert_eq!(guest_rust_buf.power, -99999.0);
    assert_eq!(guest_rust_buf.shield, 0xDEADBEEF);

    let ffi_poison_c = unsafe { host_cmd_spawn_empty(raw_queue) };
    let mut guest_c_buf = CModCustomData {
        mana: 333.25,
        level: 42,
    };
    let res_c = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_poison_c,
            c"CModCustomData".as_ptr(),
            &guest_c_buf as *const _ as *const u8,
            core::mem::size_of::<CModCustomData>(),
        )
    };
    assert_eq!(res_c, FfiResult::OK);

    // Immediately POISON guest C buffer post-enqueue!
    guest_c_buf.mana = -88888.0;
    guest_c_buf.level = -1;
    assert_eq!(guest_c_buf.mana, -88888.0);
    assert_eq!(guest_c_buf.level, -1);

    // 2. Despawn-Before-Apply Safety Test
    // Spawn entity, enqueue component insertion, and immediately enqueue despawn before apply
    let ffi_despawn_e = unsafe { host_cmd_spawn_empty(raw_queue) };
    let dummy_data = RustModCustomData {
        power: 1.0,
        shield: 1,
    };
    unsafe {
        host_cmd_insert_component(
            raw_queue,
            ffi_despawn_e,
            c"RustModCustomData".as_ptr(),
            &dummy_data as *const _ as *const u8,
            8,
        );
        host_cmd_despawn(raw_queue, ffi_despawn_e);
    }

    // 3. High-Volume Burst Enqueueing (2,000 entities)
    const BURST_COUNT: usize = 2_000;
    let mut burst_entities = Vec::with_capacity(BURST_COUNT);
    for i in 0..BURST_COUNT {
        let ffi_e = unsafe { host_cmd_spawn_empty(raw_queue) };
        assert_ne!(ffi_e, FfiEntity(0));
        burst_entities.push(ffi_e);

        if i % 2 == 0 {
            let r_data = RustModCustomData {
                power: (i as f32) + 0.5,
                shield: (i as u32) + 10,
            };
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"RustModCustomData".as_ptr(),
                    &r_data as *const _ as *const u8,
                    core::mem::size_of::<RustModCustomData>(),
                );
            }
        } else {
            let c_data = CModCustomData {
                mana: (i as f32) * 1.5,
                level: (i as i32) + 1,
            };
            unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    ffi_e,
                    c"CModCustomData".as_ptr(),
                    &c_data as *const _ as *const u8,
                    core::mem::size_of::<CModCustomData>(),
                );
            }
        }
    }

    // 4. Apply all deferred burst commands to World
    command_queue.apply(app.world_mut());

    // 5. Verify Memory Poisoning: Host stored the original bytes, NOT the poisoned bytes
    {
        let ent_r = app.world().entity(Entity::from_bits(ffi_poison_rust.0));
        let ptr_r = ent_r.get_by_id(rust_comp_id).expect("Poison test Rust entity exists");
        let data_r = unsafe { *(ptr_r.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data_r,
            RustModCustomData {
                power: 777.75,
                shield: 999,
            },
            "Host must store original bytes, unaffected by post-enqueue guest buffer poisoning"
        );

        let ent_c = app.world().entity(Entity::from_bits(ffi_poison_c.0));
        let ptr_c = ent_c.get_by_id(c_comp_id).expect("Poison test C entity exists");
        let data_c = unsafe { *(ptr_c.as_ptr() as *const CModCustomData) };
        assert_eq!(
            data_c,
            CModCustomData {
                mana: 333.25,
                level: 42,
            },
            "Host must store original bytes, unaffected by post-enqueue guest buffer poisoning"
        );
    }

    // 6. Verify Despawn-Before-Apply Safety: Entity is cleanly absent
    let bevy_despawn_e = Entity::from_bits(ffi_despawn_e.0);
    assert!(
        app.world().get_entity(bevy_despawn_e).is_err(),
        "Entity despawned before apply must not exist in world"
    );

    // 7. Verify all 2,000 Burst Entities exist and possess correct initial values
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

    // 8. Execute 1 tick: verify foreign systems mutate the burst entities
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

    // Clean teardown
    app.unload_mod(rust_mod_id).expect("Unload Rust mod");
    app.unload_mod(c_mod_id).expect("Unload C mod");
}

/// Tracks foreign destructor invocations across mod lifecycle.
static DROP_INVOCATION_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn mock_foreign_drop_fn(_ptr: *mut c_void) {
    DROP_INVOCATION_COUNT.fetch_add(1, Ordering::SeqCst);
}

/// Test 4 (Milestone 4.4): Hot-Unload, Reload Lifecycle Safety, Atomic Tombstones & Drop Trampolines
///
/// Verifies:
/// 1. Asymmetric Unload: Unloading Rust mod deactivates Rust dynamic systems via atomic tombstone
///    while C99 mod systems continue actively mutating without disruption.
/// 2. Complete Unload: Unloading C mod freezes all dynamic components in place.
/// 3. Post-Unload Despawn Safety: Despawning entities post-unload never panics, crashes, or executes
///    unmapped foreign destructor code (drop trampoline safety).
/// 4. Reload Idempotency: Reloading both mods preserves identical `ComponentId`s in `ModRegistry`,
///    rebinds drop trampolines, and registers new active systems.
/// 5. Resumption: Reloaded systems seamlessly resume mutating both pre-existing surviving entities
///    and newly spawned entities.
#[test]
fn test_milestone_4_hot_unload_reload_and_tombstone_lifecycle_safety() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);
    DROP_INVOCATION_COUNT.store(0, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // Phase 1: Initial Mod Load
    let rust_mod_id_1 = app
        .load_mod_tracked(&rust_dylib)
        .expect("Load Rust mod (phase 1)");
    let c_mod_id_1 = app
        .load_mod_tracked(&c_dylib)
        .expect("Load C mod (phase 1)");

    let (rust_comp_id, c_comp_id) = {
        let reg = app.world().resource::<ModRegistry>();
        (
            reg.get_by_name("RustModCustomData").unwrap().component_id,
            reg.get_by_name("CModCustomData").unwrap().component_id,
        )
    };

    // Register a non-POD dynamic component with a foreign drop hook into the drop trampoline pool
    let drop_test_uuid = ComponentUuid::from_name("NonPodDropTestComp");
    let non_pod_is_active = Arc::new(AtomicBool::new(true));
    let non_pod_comp_id = {
        let mut init_ctx = ModInitContext::with_world(
            Arc::new(app.world().resource::<ModRegistry>().clone()),
            Arc::clone(&non_pod_is_active),
            app.world_mut() as *mut World,
        );
        let _guard = init_ctx.make_active();
        let desc = ModComponentDescriptor {
            struct_size: core::mem::size_of::<ModComponentDescriptor>() as u32,
            name: c"NonPodDropTestComp".as_ptr(),
            uuid: drop_test_uuid,
            size: 8,
            align: 4,
            drop_fn: Some(mock_foreign_drop_fn),
            vtable_ptr: core::ptr::null(),
        };
        let res = unsafe { host_register_component(&mut init_ctx as *mut _ as *mut c_void, &desc) };
        assert_eq!(res, FfiResult::OK);
        app.world()
            .resource::<ModRegistry>()
            .get_by_name("NonPodDropTestComp")
            .unwrap()
            .component_id
    };

    // Spawn non-POD drop test entities
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

    // Despawn Entity 1 while mod/trampoline is active: foreign drop hook must execute
    app.world_mut().despawn(e_nonpod_active);
    assert_eq!(
        DROP_INVOCATION_COUNT.load(Ordering::SeqCst),
        1,
        "Active foreign drop function must execute upon entity despawn"
    );

    // Spawn entities across groups
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

    // Tick simulation for 5 ticks
    for _ in 0..5 {
        app.update();
    }

    // Verify values after 5 ticks (Rust: +50 power, +5 shield; C: +25 mana, +5 level)
    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data.power, 150.0);
        assert_eq!(data.shield, 15);

        let ent = app.world().entity(c_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 75.0);
        assert_eq!(data.level, 6);

        let ent = app.world().entity(shared_entities[0]);
        let r_data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let c_data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(r_data.power, 250.0);
        assert_eq!(r_data.shield, 25);
        assert_eq!(c_data.mana, 125.0);
        assert_eq!(c_data.level, 10);
    }

    // Phase 2: Asymmetric Unload (Unload Rust mod only)
    app.unload_mod(rust_mod_id_1).expect("Unload Rust mod");

    // Tick simulation 5 more ticks with Rust mod unloaded, C mod still active
    for _ in 0..5 {
        app.update();
    }

    // Verify asymmetric behavior:
    // Rust mod systems are inert tombstones -> values strictly frozen
    // C mod continues running -> mana +25, level +5
    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(
            data.power, 150.0,
            "RustModCustomData must be frozen after Rust mod unload"
        );
        assert_eq!(
            data.shield, 15,
            "RustModCustomData must be frozen after Rust mod unload"
        );

        let ent = app.world().entity(c_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(
            data.mana, 100.0,
            "CModCustomData must continue advancing while C mod is active"
        );
        assert_eq!(
            data.level, 11,
            "CModCustomData must continue advancing while C mod is active"
        );

        let ent = app.world().entity(shared_entities[0]);
        let r_data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let c_data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(r_data.power, 250.0, "Shared Rust data must be frozen");
        assert_eq!(r_data.shield, 25, "Shared Rust data must be frozen");
        assert_eq!(c_data.mana, 150.0, "Shared C data must advance to 150.0");
        assert_eq!(c_data.level, 15, "Shared C data must advance to 15");
    }

    // Phase 3: Unload C Mod
    app.unload_mod(c_mod_id_1).expect("Unload C mod");

    // Tick 5 more ticks: both mods are unloaded, all systems are inert tombstones
    for _ in 0..5 {
        app.update();
    }

    // Verify all values remain frozen
    {
        let ent = app.world().entity(c_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 100.0);
        assert_eq!(data.level, 11);

        let ent = app.world().entity(shared_entities[0]);
        let r_data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let c_data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(r_data.power, 250.0);
        assert_eq!(c_data.mana, 150.0);
    }

    // Phase 4: Post-Unload Despawn Safety
    // Despawn 50 entities of each group post-unload without panics or crashes
    for e in rust_only_entities.drain(50..) {
        app.world_mut().despawn(e);
    }
    for e in c_only_entities.drain(50..) {
        app.world_mut().despawn(e);
    }
    for e in shared_entities.drain(50..) {
        app.world_mut().despawn(e);
    }

    // Despawn non-POD entity post-unload:
    // Deactivate simulated mod tombstone flag; drop trampoline slot must detect is_active == false and skip foreign drop hook
    non_pod_is_active.store(false, Ordering::Release);
    app.world_mut().despawn(e_nonpod_post_unload);
    assert_eq!(
        DROP_INVOCATION_COUNT.load(Ordering::SeqCst),
        1,
        "Foreign drop hook must NOT be invoked after mod unload (averting unmapped execution)"
    );

    // Phase 5: Reload Both Mods
    let rust_mod_id_2 = app
        .load_mod_tracked(&rust_dylib)
        .expect("Reload Rust mod (phase 5)");
    let c_mod_id_2 = app
        .load_mod_tracked(&c_dylib)
        .expect("Reload C mod (phase 5)");

    // Verify component descriptor rebinding: ComponentId is preserved
    {
        let reg = app.world().resource::<ModRegistry>();
        assert_eq!(
            reg.get_by_name("RustModCustomData").unwrap().component_id,
            rust_comp_id,
            "Reloading mod must preserve existing ComponentId in ModRegistry"
        );
        assert_eq!(
            reg.get_by_name("CModCustomData").unwrap().component_id,
            c_comp_id,
            "Reloading mod must preserve existing ComponentId in ModRegistry"
        );
    }

    // Spawn 50 NEW entities possessing the dynamic components
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

    // Phase 6: Resume Simulation across both surviving entities and new entities
    for _ in 0..5 {
        app.update();
    }

    // Verify surviving entities resumed updating seamlessly:
    // Surviving Rust-only: was 150.0/15 -> +50/+5 -> 200.0/20
    // Surviving C-only: was 100.0/11 -> +25/+5 -> 125.0/16
    // Surviving Shared: Rust was 250.0/25 -> 300.0/30; C was 150.0/15 -> 175.0/20
    {
        let ent = app.world().entity(rust_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        assert_eq!(data.power, 200.0);
        assert_eq!(data.shield, 20);

        let ent = app.world().entity(c_only_entities[0]);
        let data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(data.mana, 125.0);
        assert_eq!(data.level, 16);

        let ent = app.world().entity(shared_entities[0]);
        let r_data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let c_data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(r_data.power, 300.0);
        assert_eq!(r_data.shield, 30);
        assert_eq!(c_data.mana, 175.0);
        assert_eq!(c_data.level, 20);
    }

    // Verify newly spawned entities were also processed by reloaded systems:
    // New: power was 500.0 -> 550.0; shield was 50 -> 55; mana was 200.0 -> 225.0; level was 20 -> 25
    {
        let ent = app.world().entity(new_entities[0]);
        let r_data = unsafe {
            *(ent.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData)
        };
        let c_data = unsafe {
            *(ent.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData)
        };
        assert_eq!(r_data.power, 550.0);
        assert_eq!(r_data.shield, 55);
        assert_eq!(c_data.mana, 225.0);
        assert_eq!(c_data.level, 25);
    }

    // Final clean teardown
    app.unload_mod(rust_mod_id_2)
        .expect("Unload reloaded Rust mod");
    app.unload_mod(c_mod_id_2)
        .expect("Unload reloaded C mod");

    for e in rust_only_entities
        .into_iter()
        .chain(c_only_entities)
        .chain(shared_entities)
        .chain(new_entities)
    {
        app.world_mut().despawn(e);
    }
}

