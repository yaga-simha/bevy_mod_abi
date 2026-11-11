#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use abi_contract::{ComponentUuid, RawChunkIter, RawCommandQueue};
use bevy_app::{App, Update};
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{ResMut, Resource};
use bevy_ecs::query::Changed;
use bevy_ecs::system::Query;
use common::*;
use host_runtime::{
    host_cmd_insert_component, host_cmd_spawn_empty, AbiModPlugin, DynamicCSystem, ModAppExt,
    ModRegistry,
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
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();

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

    // Clean up panic state for future tests
    reset_rust_mod_panic(&rust_dylib);
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
        assert_eq!(rust_meta.uuid, ComponentUuid::from_name("RustModCustomData"));
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
        bevy_ptr::OwningPtr::make(init_data, |ptr| {
            entity_mut.insert_by_id(rust_comp_id, ptr);
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
        bevy_ptr::OwningPtr::make(init_data, |ptr| {
            entity_mut.insert_by_id(c_comp_id, ptr);
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
        bevy_ptr::OwningPtr::make(init_rust, |ptr| {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        bevy_ptr::OwningPtr::make(init_c, |ptr| {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        e
    };

    // Record initial change ticks prior to system execution
    let initial_rust_tick = app.world().entity(e_rust).get_change_ticks_by_id(rust_comp_id).unwrap().changed;
    let initial_c_tick = app.world().entity(e_c).get_change_ticks_by_id(c_comp_id).unwrap().changed;
    let initial_both_rust_tick = app.world().entity(e_both).get_change_ticks_by_id(rust_comp_id).unwrap().changed;
    let initial_both_c_tick = app.world().entity(e_both).get_change_ticks_by_id(c_comp_id).unwrap().changed;

    // 4. Execute 1 tick of the Bevy schedule
    app.update();

    let frame1_tick = app.world().change_tick();

    // Verify mutations on Entity 1 (RustModCustomData: power +10.0, shield +1)
    {
        let entity_ref = app.world().entity(e_rust);
        let ptr = entity_ref.get_by_id(rust_comp_id).expect("e_rust must have RustModCustomData");
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
        assert_eq!(ticks.changed, frame1_tick, "Rust dynamic component must be marked changed in frame 1");
        assert!(ticks.changed > initial_rust_tick);
    }

    // Verify mutations on Entity 2 (CModCustomData: mana +5.0f, level +1)
    {
        let entity_ref = app.world().entity(e_c);
        let ptr = entity_ref.get_by_id(c_comp_id).expect("e_c must have CModCustomData");
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
        assert_eq!(ticks.changed, frame1_tick, "C99 dynamic component must be marked changed in frame 1");
        assert!(ticks.changed > initial_c_tick);
    }

    // Verify mutations on Entity 3 (Shared archetype mutated by BOTH Rust and C99 systems)
    {
        let entity_ref = app.world().entity(e_both);

        let ptr_rust = entity_ref.get_by_id(rust_comp_id).expect("e_both must have RustModCustomData");
        let data_rust = unsafe { *(ptr_rust.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data_rust,
            RustModCustomData {
                power: 210.0,
                shield: 11,
            },
            "Shared entity Rust component must be mutated by Rust mod"
        );

        let ptr_c = entity_ref.get_by_id(c_comp_id).expect("e_both must have CModCustomData");
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
        assert_eq!(ticks_rust.changed, frame1_tick);
        assert_eq!(ticks_c.changed, frame1_tick);
        assert!(ticks_rust.changed > initial_both_rust_tick);
        assert!(ticks_c.changed > initial_both_c_tick);
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
        let data = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data, RustModCustomData { power: 150.0, shield: 55 });

        let entity_ref = app.world().entity(e_c);
        let data = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data, CModCustomData { mana: 50.0, level: 8 });

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r, RustModCustomData { power: 250.0, shield: 15 });
        assert_eq!(data_c, CModCustomData { mana: 75.0, level: 12 });
    }

    // 5. Mod unloading & lifecycle safety
    app.unload_mod(rust_mod_id).expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id).expect("C mod must unload cleanly");

    // Post-unload ticks: components remain intact, values frozen (no further system execution)
    app.update();
    app.update();

    {
        let entity_ref = app.world().entity(e_rust);
        let data = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data, RustModCustomData { power: 150.0, shield: 55 }, "Values must remain frozen after mod unload");

        let entity_ref = app.world().entity(e_c);
        let data = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data, CModCustomData { mana: 50.0, level: 8 }, "Values must remain frozen after mod unload");

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r, RustModCustomData { power: 250.0, shield: 15 });
        assert_eq!(data_c, CModCustomData { mana: 75.0, level: 12 });
    }

    // Despawn all entities post-unload without panics or crashes
    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
    app.world_mut().despawn(e_both);
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
        assert_eq!(rust_meta.uuid, ComponentUuid::from_name("RustModCustomData"));
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
        bevy_ptr::OwningPtr::make(init_data, |ptr| {
            entity_mut.insert_by_id(rust_comp_id, ptr);
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
        bevy_ptr::OwningPtr::make(init_data, |ptr| {
            entity_mut.insert_by_id(c_comp_id, ptr);
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
        bevy_ptr::OwningPtr::make(init_rust, |ptr| {
            entity_mut.insert_by_id(rust_comp_id, ptr);
        });
        bevy_ptr::OwningPtr::make(init_c, |ptr| {
            entity_mut.insert_by_id(c_comp_id, ptr);
        });
        e
    };

    // Record initial change ticks prior to system execution
    let initial_rust_tick = app.world().entity(e_rust).get_change_ticks_by_id(rust_comp_id).unwrap().changed;
    let initial_c_tick = app.world().entity(e_c).get_change_ticks_by_id(c_comp_id).unwrap().changed;
    let initial_both_rust_tick = app.world().entity(e_both).get_change_ticks_by_id(rust_comp_id).unwrap().changed;
    let initial_both_c_tick = app.world().entity(e_both).get_change_ticks_by_id(c_comp_id).unwrap().changed;

    // 4. Execute 1 tick of the Bevy schedule
    app.update();

    let frame1_tick = app.world().change_tick();

    // Verify mutations on Entity 1 (RustModCustomData: power +10.0, shield +1)
    {
        let entity_ref = app.world().entity(e_rust);
        let ptr = entity_ref.get_by_id(rust_comp_id).expect("e_rust must have RustModCustomData");
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
        assert_eq!(ticks.changed, frame1_tick, "Rust dynamic component must be marked changed in frame 1");
        assert!(ticks.changed > initial_rust_tick);
    }

    // Verify mutations on Entity 2 (CModCustomData: mana +5.0f, level +1)
    {
        let entity_ref = app.world().entity(e_c);
        let ptr = entity_ref.get_by_id(c_comp_id).expect("e_c must have CModCustomData");
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
        assert_eq!(ticks.changed, frame1_tick, "C99 dynamic component must be marked changed in frame 1");
        assert!(ticks.changed > initial_c_tick);
    }

    // Verify mutations on Entity 3 (Shared archetype mutated by BOTH Rust and C99 systems)
    {
        let entity_ref = app.world().entity(e_both);

        let ptr_rust = entity_ref.get_by_id(rust_comp_id).expect("e_both must have RustModCustomData");
        let data_rust = unsafe { *(ptr_rust.as_ptr() as *const RustModCustomData) };
        assert_eq!(
            data_rust,
            RustModCustomData {
                power: 210.0,
                shield: 11,
            },
            "Shared entity Rust component must be mutated by Rust mod"
        );

        let ptr_c = entity_ref.get_by_id(c_comp_id).expect("e_both must have CModCustomData");
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
        assert_eq!(ticks_rust.changed, frame1_tick);
        assert_eq!(ticks_c.changed, frame1_tick);
        assert!(ticks_rust.changed > initial_both_rust_tick);
        assert!(ticks_c.changed > initial_both_c_tick);
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
        let data = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data, RustModCustomData { power: 150.0, shield: 55 });

        let entity_ref = app.world().entity(e_c);
        let data = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data, CModCustomData { mana: 50.0, level: 8 });

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r, RustModCustomData { power: 250.0, shield: 15 });
        assert_eq!(data_c, CModCustomData { mana: 75.0, level: 12 });
    }

    // 5. Mod unloading & lifecycle safety
    app.unload_mod(rust_mod_id).expect("Rust mod must unload cleanly");
    app.unload_mod(c_mod_id).expect("C mod must unload cleanly");

    // Post-unload ticks: components remain intact, values frozen (no further system execution)
    app.update();
    app.update();

    {
        let entity_ref = app.world().entity(e_rust);
        let data = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        assert_eq!(data, RustModCustomData { power: 150.0, shield: 55 }, "Values must remain frozen after mod unload");

        let entity_ref = app.world().entity(e_c);
        let data = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data, CModCustomData { mana: 50.0, level: 8 }, "Values must remain frozen after mod unload");

        let entity_ref = app.world().entity(e_both);
        let data_r = unsafe { *(entity_ref.get_by_id(rust_comp_id).unwrap().as_ptr() as *const RustModCustomData) };
        let data_c = unsafe { *(entity_ref.get_by_id(c_comp_id).unwrap().as_ptr() as *const CModCustomData) };
        assert_eq!(data_r, RustModCustomData { power: 250.0, shield: 15 });
        assert_eq!(data_c, CModCustomData { mana: 75.0, level: 12 });
    }

    // Despawn all entities post-unload without panics or crashes
    app.world_mut().despawn(e_rust);
    app.world_mut().despawn(e_c);
    app.world_mut().despawn(e_both);
}
