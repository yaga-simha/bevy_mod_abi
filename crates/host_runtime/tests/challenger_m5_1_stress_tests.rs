//! Empirical Adversarial Verification & Stress Test Suite for Milestone 5
//!
//! Rigorous stress testing of:
//! 1. Chunk iteration boundary conditions (0 entities, 1 entity, multi-chunk archetypes >16,384 entities).
//! 2. Component access permission enforcement (READ vs WRITE vs FILTER_ONLY vs WITHOUT, invalid column indices).
//! 3. Change detection granularity (blanket chunk mutation vs selective row-level `host_iter_mark_changed`).
//! 4. High-volume deferred commands execution (2,000 rapid spawns, inserts, and 1,000 despawns).
//! 5. Command bridge error resilience (null pointers, bad entity bits, mismatched sizes, unregistered components, invalid UTF-8).
//! 6. Multi-query safety & nested iteration (self-conflict detection, compatible multi-queries, nested loops with `host_iter_reset`).
//! 7. Cross-language binary struct memory layout matching (C99 vs Rust repr(C) byte-for-byte).
//! 8. Cross-language concurrent runtime stress (Example 3 C99 + Rust mods running simultaneously under heavy entity loads and dynamic reload cycles).

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]
#![allow(clippy::default_constructed_unit_structs)]

mod common;

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::{Query, System};
use common::*;
use host_runtime::{
    commands::CommandQueueBridge, host_cmd_despawn, host_cmd_insert_component,
    host_cmd_spawn_empty, host_iter_column_mutable, host_iter_column_readonly, host_iter_entities,
    host_iter_mark_changed, host_iter_next_chunk, host_iter_reset, AbiModPlugin,
    ComponentAccessDescOwned, DynamicCSystem, ModAppExt, ModRegistry,
};
use mod_derive::ModExposable;
use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

// ==============================================================================
// Component Definitions for Challenger Tests
// ==============================================================================

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct DroneTag {
    pub squad: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct PlayerTag {
    pub id: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct ColorRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

// ==============================================================================
// TEST 1: Chunk Iteration Boundaries (0 Entities & 1 Entity)
// ==============================================================================

static ZERO_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn zero_entity_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    ZERO_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

    // With 0 entities, next_chunk must immediately return 0
    let count = unsafe { host_iter_next_chunk(iter) };
    assert_eq!(count, 0, "Empty world chunk iteration must return 0");

    // All column pointers and entity pointers must be null
    let readonly_ptr = unsafe { host_iter_column_readonly(iter, 0) };
    assert!(
        readonly_ptr.is_null(),
        "Readonly column pointer must be null when chunk has 0 entities"
    );

    let mutable_ptr = unsafe { host_iter_column_mutable(iter, 1) };
    assert!(
        mutable_ptr.is_null(),
        "Mutable column pointer must be null when chunk has 0 entities"
    );

    let entities_ptr = unsafe { host_iter_entities(iter) };
    assert!(
        entities_ptr.is_null(),
        "Entities pointer must be null when chunk has 0 entities"
    );

    // Iter reset must succeed cleanly
    let reset_res = unsafe { host_iter_reset(iter) };
    assert_eq!(reset_res, FfiResult::OK);

    // Iterating again after reset must still return 0
    let count_again = unsafe { host_iter_next_chunk(iter) };
    assert_eq!(count_again, 0);

    FfiResult::OK
}

static SINGLE_ENTITY_VISITS: AtomicUsize = AtomicUsize::new(0);
static SINGLE_ENTITY_ID_BITS: AtomicU32 = AtomicU32::new(0);

unsafe extern "C" fn single_entity_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    SINGLE_ENTITY_VISITS.fetch_add(1, Ordering::SeqCst);

    let count = unsafe { host_iter_next_chunk(iter) };
    assert_eq!(
        count, 1,
        "Single entity world chunk iteration must return exactly 1"
    );

    // Entity pointer must be non-null and valid
    let entities_ptr = unsafe { host_iter_entities(iter) };
    assert!(!entities_ptr.is_null(), "Entities pointer must not be null");
    let entity = unsafe { *entities_ptr };
    SINGLE_ENTITY_ID_BITS.store(entity.0 as u32, Ordering::SeqCst);

    // Read column 0 (Position)
    let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
    assert!(!pos_ptr.is_null(), "Position pointer must be non-null");
    let pos = unsafe { *pos_ptr };
    assert_eq!(
        pos,
        Position {
            x: 42.0,
            y: 100.0,
            z: -5.0
        }
    );

    // Mutate column 1 (Velocity)
    let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
    assert!(!vel_ptr.is_null(), "Velocity pointer must be non-null");
    unsafe {
        (*vel_ptr).x += 10.0;
        (*vel_ptr).y += 20.0;
        (*vel_ptr).z += 30.0;
    }

    // Exhaust iterator
    let next_count = unsafe { host_iter_next_chunk(iter) };
    assert_eq!(
        next_count, 0,
        "Single entity world must be exhausted on second next_chunk"
    );

    FfiResult::OK
}

#[test]
fn test_stress_chunk_iteration_zero_and_single_entity() {
    // --- Part A: Zero entities ---
    {
        ZERO_ENTITY_VISITS.store(0, Ordering::SeqCst);
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ];

        let system = DynamicCSystem::new(
            "zero_entity_system".to_string(),
            query_descs,
            zero_entity_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, system);

        for _ in 0..5 {
            app.update();
        }
        assert_eq!(ZERO_ENTITY_VISITS.load(Ordering::SeqCst), 5);
    }

    // --- Part B: Single entity ---
    {
        SINGLE_ENTITY_VISITS.store(0, Ordering::SeqCst);
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: 42.0,
                    y: 100.0,
                    z: -5.0,
                },
                Velocity {
                    x: 1.0,
                    y: 2.0,
                    z: 3.0,
                },
            ))
            .id();

        let registry = app.world().resource::<ModRegistry>().clone();
        let query_descs = vec![
            ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
        ];

        let system = DynamicCSystem::new(
            "single_entity_system".to_string(),
            query_descs,
            single_entity_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, system);

        app.update();
        assert_eq!(SINGLE_ENTITY_VISITS.load(Ordering::SeqCst), 1);
        assert_eq!(
            SINGLE_ENTITY_ID_BITS.load(Ordering::SeqCst),
            (e.to_bits() & 0xFFFF_FFFF) as u32
        );

        let vel = *app.world().get::<Velocity>(e).unwrap();
        assert_eq!(
            vel,
            Velocity {
                x: 11.0,
                y: 22.0,
                z: 33.0
            }
        );
    }
}

// ==============================================================================
// TEST 2: Multi-Chunk Archetypes (>16,384 entities: 25,000 entities across 5 archetypes)
// ==============================================================================

static MULTI_CHUNK_TOTAL_ENTITIES: AtomicUsize = AtomicUsize::new(0);
static MULTI_CHUNK_TOTAL_CHUNKS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn multi_chunk_scale_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut total_entities = 0;
    let mut chunks = 0;

    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        chunks += 1;
        total_entities += count;

        let entities_ptr = unsafe { host_iter_entities(iter) };
        assert!(!entities_ptr.is_null());

        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const Position;
        assert!(!pos_ptr.is_null());
        assert_eq!((pos_ptr as usize) % core::mem::align_of::<Position>(), 0);

        let vel_ptr = unsafe { host_iter_column_mutable(iter, 1) } as *mut Velocity;
        assert!(!vel_ptr.is_null());
        assert_eq!((vel_ptr as usize) % core::mem::align_of::<Velocity>(), 0);

        // Mutate each entity's velocity
        for i in 0..count {
            let p = unsafe { *pos_ptr.add(i) };
            let v = unsafe { &mut *vel_ptr.add(i) };
            v.x += 1.0;
            v.y += p.x * 0.001;
        }
    }

    MULTI_CHUNK_TOTAL_ENTITIES.store(total_entities, Ordering::SeqCst);
    MULTI_CHUNK_TOTAL_CHUNKS.store(chunks, Ordering::SeqCst);

    // Test iterator reset
    let reset_status = unsafe { host_iter_reset(iter) };
    assert_eq!(reset_status, FfiResult::OK);

    let mut re_count = 0;
    let mut re_chunks = 0;
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        re_chunks += 1;
        re_count += count;
    }
    assert_eq!(
        re_count, total_entities,
        "Iterator reset must re-traverse identical entity count"
    );
    assert_eq!(
        re_chunks, chunks,
        "Iterator reset must re-traverse identical chunk count"
    );

    FfiResult::OK
}

#[test]
fn test_stress_chunk_iteration_multi_chunk_archetypes_25k() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<Mass>();
    app.register_mod_component::<Armor>();

    // Spawn 25,000 entities partitioned across 5 distinct archetypes (5,000 entities each)
    const PER_ARCH: usize = 5_000;
    let mut spawned_entities = Vec::with_capacity(PER_ARCH * 5);

    // Archetype 1: Position, Velocity
    for i in 0..PER_ARCH {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
            ))
            .id();
        spawned_entities.push(e);
    }
    // Archetype 2: Position, Velocity, CombatHealth
    for i in 0..PER_ARCH {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
                CombatHealth {
                    current: 100,
                    max: 100,
                },
            ))
            .id();
        spawned_entities.push(e);
    }
    // Archetype 3: Position, Velocity, Mass
    for i in 0..PER_ARCH {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
                Mass { value: 50.0 },
            ))
            .id();
        spawned_entities.push(e);
    }
    // Archetype 4: Position, Velocity, Armor
    for i in 0..PER_ARCH {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
                Armor { defense: 25 },
            ))
            .id();
        spawned_entities.push(e);
    }
    // Archetype 5: Position, Velocity, CombatHealth, Armor
    for i in 0..PER_ARCH {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 20.0,
                    z: 30.0,
                },
                CombatHealth {
                    current: 200,
                    max: 200,
                },
                Armor { defense: 50 },
            ))
            .id();
        spawned_entities.push(e);
    }

    assert_eq!(spawned_entities.len(), 25_000);

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
    ];

    let system = DynamicCSystem::new(
        "multi_chunk_25k_system".to_string(),
        query_descs,
        multi_chunk_scale_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );
    app.add_systems(Update, system);

    app.update();

    assert_eq!(MULTI_CHUNK_TOTAL_ENTITIES.load(Ordering::SeqCst), 25_000);
    assert_eq!(MULTI_CHUNK_TOTAL_CHUNKS.load(Ordering::SeqCst), 5);

    // Verify mutations across sample entities
    for &e in &[
        spawned_entities[0],
        spawned_entities[5000],
        spawned_entities[10000],
        spawned_entities[24999],
    ] {
        let v = *app.world().get::<Velocity>(e).unwrap();
        assert_eq!(v.x, 11.0);
    }
}

// ==============================================================================
// TEST 3: Component Access Permission Enforcement
// ==============================================================================

static PERMISSIONS_VERIFIED: AtomicBool = AtomicBool::new(false);

unsafe extern "C" fn access_permissions_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let count = unsafe { host_iter_next_chunk(iter) };
    if count == 0 {
        return FfiResult::OK;
    }

    // Col 0: Position (READ)
    let pos_readonly = unsafe { host_iter_column_readonly(iter, 0) };
    assert!(
        !pos_readonly.is_null(),
        "Readonly access on READ column must succeed"
    );
    let pos_mut = unsafe { host_iter_column_mutable(iter, 0) };
    assert!(
        pos_mut.is_null(),
        "Mutable access on READ column must be DENIED (null)"
    );
    let pos_mark = unsafe { host_iter_mark_changed(iter, 0, 0) };
    assert_eq!(
        pos_mark,
        FfiResult::PERMISSION_DENIED,
        "Mark changed on READ column must return PERMISSION_DENIED"
    );

    // Col 1: Velocity (WRITE)
    let vel_readonly = unsafe { host_iter_column_readonly(iter, 1) };
    assert!(
        !vel_readonly.is_null(),
        "Readonly access on WRITE column must succeed"
    );
    let vel_mut = unsafe { host_iter_column_mutable(iter, 1) };
    assert!(
        !vel_mut.is_null(),
        "Mutable access on WRITE column must succeed"
    );
    let vel_mark = unsafe { host_iter_mark_changed(iter, 1, 0) };
    assert_eq!(
        vel_mark,
        FfiResult::OK,
        "Mark changed on WRITE column must succeed"
    );

    // Col 2: CombatHealth (FILTER_ONLY)
    let health_readonly = unsafe { host_iter_column_readonly(iter, 2) };
    assert!(
        health_readonly.is_null(),
        "Read access on FILTER_ONLY column must return null"
    );
    let health_mut = unsafe { host_iter_column_mutable(iter, 2) };
    assert!(
        health_mut.is_null(),
        "Mutable access on FILTER_ONLY column must return null"
    );

    // Col 3: Mass (WITHOUT)
    let mass_readonly = unsafe { host_iter_column_readonly(iter, 3) };
    assert!(
        mass_readonly.is_null(),
        "Read access on WITHOUT column must return null"
    );
    let mass_mut = unsafe { host_iter_column_mutable(iter, 3) };
    assert!(
        mass_mut.is_null(),
        "Mutable access on WITHOUT column must return null"
    );

    // Out of bounds indices
    let oob_readonly = unsafe { host_iter_column_readonly(iter, 4) };
    assert!(oob_readonly.is_null());
    let oob_mut = unsafe { host_iter_column_mutable(iter, 32) };
    assert!(oob_mut.is_null());
    let oob_mark = unsafe { host_iter_mark_changed(iter, 100, 0) };
    assert_eq!(oob_mark, FfiResult::INVALID_COMPONENT);

    // Row out of bounds
    let row_oob_mark = unsafe { host_iter_mark_changed(iter, 1, 999_999) };
    assert_eq!(row_oob_mark, FfiResult::INTERNAL_ERROR);

    PERMISSIONS_VERIFIED.store(true, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_stress_component_access_permissions() {
    PERMISSIONS_VERIFIED.store(false, Ordering::SeqCst);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<Mass>();

    // Entity matching: Position, Velocity, CombatHealth (WITHOUT Mass)
    app.world_mut().spawn((
        Position {
            x: 1.0,
            y: 2.0,
            z: 3.0,
        },
        Velocity {
            x: 0.1,
            y: 0.2,
            z: 0.3,
        },
        CombatHealth {
            current: 100,
            max: 100,
        },
    ));

    // Entity that must be filtered out because it has Mass
    app.world_mut().spawn((
        Position {
            x: 99.0,
            y: 99.0,
            z: 99.0,
        },
        Velocity {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        },
        CombatHealth {
            current: 100,
            max: 100,
        },
        Mass { value: 10.0 },
    ));

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::READ,
        },
        ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        ComponentAccessDescOwned {
            name: "CombatHealth".to_string(),
            mode: ComponentAccessMode::FILTER_ONLY,
        },
        ComponentAccessDescOwned {
            name: "Mass".to_string(),
            mode: ComponentAccessMode::WITHOUT,
        },
    ];

    let system = DynamicCSystem::new(
        "access_permissions_system".to_string(),
        query_descs,
        access_permissions_callback,
        core::ptr::null_mut(),
        Arc::new(registry),
    );
    app.add_systems(Update, system);

    app.update();
    assert!(PERMISSIONS_VERIFIED.load(Ordering::SeqCst));
}

// ==============================================================================
// TEST 4: Change Detection Granularity (Blanket vs Selective row-level)
// ==============================================================================

static BLANKET_CHANGE_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn blanket_mutation_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    let count = unsafe { host_iter_next_chunk(iter) };
    if count > 0 {
        // Calling column_mutable marks entire chunk as changed
        let vel_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut Velocity;
        assert!(!vel_ptr.is_null());
        for i in 0..count {
            unsafe {
                (*vel_ptr.add(i)).x += 1.0;
            }
        }
        BLANKET_CHANGE_CALL_COUNT.fetch_add(count, Ordering::SeqCst);
    }
    FfiResult::OK
}

static SELECTIVE_CHANGE_CALL_COUNT: AtomicUsize = AtomicUsize::new(0);
static SELECTIVE_MODE: AtomicUsize = AtomicUsize::new(0); // 0 = no-op, 1 = selective mark row 7
const TARGET_ROW: usize = 7;

unsafe extern "C" fn selective_mutation_callback(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        if SELECTIVE_MODE.load(Ordering::SeqCst) == 1 && count > TARGET_ROW {
            let vel_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *mut Velocity;
            assert!(!vel_ptr.is_null());
            unsafe {
                (*vel_ptr.add(TARGET_ROW)).x = 999.0;
            }
            let res = unsafe { host_iter_mark_changed(iter, 0, TARGET_ROW) };
            assert_eq!(res, FfiResult::OK);
            SELECTIVE_CHANGE_CALL_COUNT.fetch_add(1, Ordering::SeqCst);
        } else {
            // Readonly access without marking ticks
            let _ = unsafe { host_iter_column_readonly(iter, 0) };
        }
    }
    FfiResult::OK
}

static CHANGED_VELOCITY_DETECTIONS: AtomicUsize = AtomicUsize::new(0);

fn count_changed_velocities(query: Query<&Velocity, Changed<Velocity>>) {
    let count = query.iter().count();
    CHANGED_VELOCITY_DETECTIONS.store(count, Ordering::SeqCst);
}

#[test]
fn test_stress_change_detection_granularity() {
    // --- Phase 1: Blanket change detection ---
    {
        CHANGED_VELOCITY_DETECTIONS.store(0, Ordering::SeqCst);
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Velocity>();

        for i in 0..50 {
            app.world_mut().spawn(Velocity {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            });
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let system = DynamicCSystem::new(
            "blanket_system".to_string(),
            vec![ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            blanket_mutation_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, system);
        app.add_systems(Update, count_changed_velocities);

        // Tick 1: baseline
        app.update();
        // Tick 2: mutation via column_mutable -> all 50 entities detected
        app.update();
        assert_eq!(CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst), 50);
    }

    // --- Phase 2: Selective row-level change detection ---
    {
        CHANGED_VELOCITY_DETECTIONS.store(0, Ordering::SeqCst);
        SELECTIVE_MODE.store(0, Ordering::SeqCst);
        let mut app = App::new();
        app.add_plugins(AbiModPlugin::default());
        app.register_mod_component::<Velocity>();

        let mut entities = Vec::new();
        for i in 0..50 {
            let e = app
                .world_mut()
                .spawn(Velocity {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                })
                .id();
            entities.push(e);
        }

        let registry = app.world().resource::<ModRegistry>().clone();
        let system = DynamicCSystem::new(
            "selective_system".to_string(),
            vec![ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            selective_mutation_callback,
            core::ptr::null_mut(),
            Arc::new(registry),
        );
        app.add_systems(Update, (system, count_changed_velocities).chain());

        // Frame 1: Initial spawn causes Bevy to detect all 50 entities as added/changed
        app.update();
        assert_eq!(CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst), 50);

        // Frame 2: Mode = 0 (no mutation). Must produce 0 changed entities (clean baseline)
        app.update();
        assert_eq!(
            CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst),
            0,
            "Non-mutating frame must produce 0 Changed<Velocity> detections"
        );

        // Frame 3: Mode = 1 (selectively mark ONLY TARGET_ROW changed)
        SELECTIVE_MODE.store(1, Ordering::SeqCst);
        app.update();

        // Exactly 1 entity must be detected as Changed<Velocity>!
        assert_eq!(
            CHANGED_VELOCITY_DETECTIONS.load(Ordering::SeqCst),
            1,
            "Selective change detection must flag exactly 1 entity, not the entire chunk"
        );

        let target_vel = *app.world().get::<Velocity>(entities[TARGET_ROW]).unwrap();
        assert_eq!(target_vel.x, 999.0);
    }
}

// ==============================================================================
// TEST 5: High-Volume Deferred Commands Execution (2,000 Spawns, Inserts, 1,000 Despawns)
// ==============================================================================

const BATCH_SIZE: usize = 2_000;
static SPAWNED_FFI_IDS: std::sync::Mutex<Vec<FfiEntity>> = std::sync::Mutex::new(Vec::new());

unsafe extern "C" fn mass_spawn_callback(
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    assert!(!commands.is_null());
    let mut ids = Vec::with_capacity(BATCH_SIZE);

    let pos_name = CString::new("Position").unwrap();
    let health_name = CString::new("CombatHealth").unwrap();

    for i in 0..BATCH_SIZE {
        let ffi_ent = unsafe { host_cmd_spawn_empty(commands) };
        assert_ne!(ffi_ent.0, 0);

        let pos = Position {
            x: i as f32,
            y: (i * 2) as f32,
            z: 0.0,
        };
        let res_pos = unsafe {
            host_cmd_insert_component(
                commands,
                ffi_ent,
                pos_name.as_ptr(),
                &pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        };
        assert_eq!(res_pos, FfiResult::OK);

        let health = CombatHealth {
            current: 100,
            max: 100,
        };
        let res_health = unsafe {
            host_cmd_insert_component(
                commands,
                ffi_ent,
                health_name.as_ptr(),
                &health as *const CombatHealth as *const u8,
                core::mem::size_of::<CombatHealth>(),
            )
        };
        assert_eq!(res_health, FfiResult::OK);

        ids.push(ffi_ent);
    }

    *SPAWNED_FFI_IDS.lock().unwrap() = ids;
    FfiResult::OK
}

unsafe extern "C" fn mass_despawn_and_insert_callback(
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    assert!(!commands.is_null());
    let ids = SPAWNED_FFI_IDS.lock().unwrap().clone();
    let vel_name = CString::new("Velocity").unwrap();

    for (idx, &ffi_ent) in ids.iter().enumerate() {
        if idx % 2 == 0 {
            // Despawn even indices
            let res = unsafe { host_cmd_despawn(commands, ffi_ent) };
            assert_eq!(res, FfiResult::OK);
        } else {
            // Insert Velocity on odd indices
            let vel = Velocity {
                x: idx as f32,
                y: -1.0,
                z: 1.0,
            };
            let res = unsafe {
                host_cmd_insert_component(
                    commands,
                    ffi_ent,
                    vel_name.as_ptr(),
                    &vel as *const Velocity as *const u8,
                    core::mem::size_of::<Velocity>(),
                )
            };
            assert_eq!(res, FfiResult::OK);
        }
    }
    FfiResult::OK
}

#[test]
fn test_stress_deferred_commands_high_volume() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<Velocity>();

    let registry = app.world().resource::<ModRegistry>().clone();

    // System 1: Mass spawn
    let spawn_system = DynamicCSystem::new(
        "mass_spawn".to_string(),
        Vec::new(),
        mass_spawn_callback,
        core::ptr::null_mut(),
        Arc::new(registry.clone()),
    );
    app.add_systems(Update, spawn_system);

    // Tick 1: Spawns queued and applied
    app.update();

    let spawned_ids = SPAWNED_FFI_IDS.lock().unwrap().clone();
    assert_eq!(spawned_ids.len(), BATCH_SIZE);

    // Verify all 2,000 entities exist with components
    for (i, &ffi_ent) in spawned_ids.iter().enumerate() {
        let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
        let pos = *app
            .world()
            .get::<Position>(bevy_ent)
            .expect("Entity must have Position");
        let health = *app
            .world()
            .get::<CombatHealth>(bevy_ent)
            .expect("Entity must have CombatHealth");
        assert_eq!(pos.x, i as f32);
        assert_eq!(health.current, 100);
    }

    // Replace system with mass despawn/insert
    let mut app2 = App::new();
    app2.add_plugins(AbiModPlugin::default());
    app2.register_mod_component::<Position>();
    app2.register_mod_component::<CombatHealth>();
    app2.register_mod_component::<Velocity>();

    // Copy world entities over
    for (i, &ffi_ent) in spawned_ids.iter().enumerate() {
        let e = app2
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: (i * 2) as f32,
                    z: 0.0,
                },
                CombatHealth {
                    current: 100,
                    max: 100,
                },
            ))
            .id();
        assert_eq!(e.to_bits(), ffi_ent.to_bits());
    }

    let registry2 = app2.world().resource::<ModRegistry>().clone();
    let despawn_system = DynamicCSystem::new(
        "mass_despawn".to_string(),
        Vec::new(),
        mass_despawn_and_insert_callback,
        core::ptr::null_mut(),
        Arc::new(registry2),
    );
    app2.add_systems(Update, despawn_system);

    // Tick 2: Despawns and inserts queued and applied
    app2.update();

    // Verify results
    for (idx, &ffi_ent) in spawned_ids.iter().enumerate() {
        let bevy_ent = Entity::try_from_bits(ffi_ent.to_bits()).unwrap();
        if idx % 2 == 0 {
            // Despawned: entity mut should fail
            assert!(
                app2.world().get_entity(bevy_ent).is_err(),
                "Even entity {} must be despawned",
                idx
            );
        } else {
            // Surviving: must have Position, CombatHealth, and Velocity
            assert!(app2.world().get::<Position>(bevy_ent).is_some());
            assert!(app2.world().get::<CombatHealth>(bevy_ent).is_some());
            let vel = *app2
                .world()
                .get::<Velocity>(bevy_ent)
                .expect("Odd entity must have Velocity");
            assert_eq!(vel.x, idx as f32);
        }
    }
}

// ==============================================================================
// TEST 6: Command Bridge Error Resilience (Adversarial Inputs)
// ==============================================================================

#[test]
fn test_stress_deferred_commands_adversarial_inputs() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();

    let mut queue = bevy_ecs::world::CommandQueue::default();
    let registry = app.world().resource::<ModRegistry>().clone();
    let cell = app.world_mut().as_unsafe_world_cell();

    let mut bridge = CommandQueueBridge::new(&mut queue, cell.entity_allocator(), &registry);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let valid_entity = unsafe { host_cmd_spawn_empty(raw_queue) };
    assert_ne!(valid_entity.0, 0);

    let dummy_pos = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let pos_name = CString::new("Position").unwrap();
    let unknown_name = CString::new("NonExistentComponent").unwrap();

    // 1. Null queue pointer
    assert_eq!(
        unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) },
        FfiEntity(0)
    );
    assert_eq!(
        unsafe { host_cmd_despawn(core::ptr::null_mut(), valid_entity) },
        FfiResult::NULL_POINTER
    );
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                core::ptr::null_mut(),
                valid_entity,
                pos_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::NULL_POINTER
    );

    // 2. Null component name
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_entity,
                core::ptr::null(),
                &dummy_pos as *const Position as *const u8,
                core::mem::size_of::<Position>(),
            )
        },
        FfiResult::NULL_POINTER
    );

    // 3. Null data pointer with len > 0
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_entity,
                pos_name.as_ptr(),
                core::ptr::null(),
                12,
            )
        },
        FfiResult::NULL_POINTER
    );

    // 4. Unregistered component name
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_entity,
                unknown_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                12,
            )
        },
        FfiResult::INVALID_COMPONENT
    );

    // 5. Component size mismatch (len != registered size)
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_entity,
                pos_name.as_ptr(),
                &dummy_pos as *const Position as *const u8,
                999, // Expected 12
            )
        },
        FfiResult::INTERNAL_ERROR
    );

    // 6. Invalid entity bits on despawn:
    // Generation 0 returns None from Entity::try_from_bits -> INTERNAL_ERROR
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, FfiEntity(0)) },
        FfiResult::INTERNAL_ERROR
    );
    // Non-existent entity with non-zero generation returns OK (queued safely) and safely no-ops on flush
    assert_eq!(
        unsafe { host_cmd_despawn(raw_queue, FfiEntity(u64::MAX)) },
        FfiResult::OK
    );

    // 7. Invalid UTF-8 in component name
    let invalid_utf8_name = [0xFFu8, 0xFEu8, 0x00u8];
    assert_eq!(
        unsafe {
            host_cmd_insert_component(
                raw_queue,
                valid_entity,
                invalid_utf8_name.as_ptr() as *const c_char,
                &dummy_pos as *const Position as *const u8,
                12,
            )
        },
        FfiResult::INVALID_UTF8
    );
}

// ==============================================================================
// TEST 7: Multi-Query Safety & Nested Iteration with Reset
// ==============================================================================

static NESTED_INTERACTION_COUNT: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn nested_pairwise_callback(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    assert_eq!(iter_count, 2);
    let iter_drones = unsafe { *iters };
    let iter_players = unsafe { *iters.add(1) };

    let mut interactions = 0;

    // Outer loop: Drones
    loop {
        let drone_count = unsafe { host_iter_next_chunk(iter_drones) };
        if drone_count == 0 {
            break;
        }

        for _ in 0..drone_count {
            // Inner loop: Players (requires iter_reset before each traversal)
            let reset_res = unsafe { host_iter_reset(iter_players) };
            assert_eq!(reset_res, FfiResult::OK);

            loop {
                let player_count = unsafe { host_iter_next_chunk(iter_players) };
                if player_count == 0 {
                    break;
                }
                interactions += player_count;
            }
        }
    }

    NESTED_INTERACTION_COUNT.store(interactions, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn test_stress_multi_query_safety_and_nesting() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<DroneTag>();
    app.register_mod_component::<PlayerTag>();

    let registry = Arc::new(app.world().resource::<ModRegistry>().clone());

    // 1. Self-conflict detection test: Query 0 writes Position, Query 1 writes Position
    {
        let conflicting_specs = vec![
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
        ];
        let mut sys = DynamicCSystem::new_multi_query(
            "self_conflict_write_write".to_string(),
            conflicting_specs,
            nested_pairwise_callback,
            core::ptr::null_mut(),
            Arc::clone(&registry),
        );
        sys.initialize(app.world_mut());
        assert!(
            sys.has_self_conflict(),
            "System with Write-Write collision must flag self_conflict"
        );
        assert!(
            !sys.is_enabled(),
            "Conflicting system must be automatically disabled"
        );
    }

    // 2. Self-conflict detection test: Query 0 writes Position, Query 1 reads Position
    {
        let conflicting_specs = vec![
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            }],
            vec![ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::READ,
            }],
        ];
        let mut sys = DynamicCSystem::new_multi_query(
            "self_conflict_write_read".to_string(),
            conflicting_specs,
            nested_pairwise_callback,
            core::ptr::null_mut(),
            Arc::clone(&registry),
        );
        sys.initialize(app.world_mut());
        assert!(
            sys.has_self_conflict(),
            "System with Write-Read collision must flag self_conflict"
        );
        assert!(!sys.is_enabled());
    }

    // 3. Compatible nested execution: 10 Drones and 5 Players -> 50 interactions
    {
        for i in 0..10 {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                DroneTag { squad: 1 },
            ));
        }
        for i in 0..5 {
            app.world_mut().spawn((
                Position {
                    x: 100.0 + i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                PlayerTag { id: i as u32 },
            ));
        }

        let compatible_specs = vec![
            vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "DroneTag".to_string(),
                    mode: ComponentAccessMode::FILTER_ONLY,
                },
            ],
            vec![
                ComponentAccessDescOwned {
                    name: "Position".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "PlayerTag".to_string(),
                    mode: ComponentAccessMode::FILTER_ONLY,
                },
            ],
        ];

        let sys = DynamicCSystem::new_multi_query(
            "nested_pairwise".to_string(),
            compatible_specs,
            nested_pairwise_callback,
            core::ptr::null_mut(),
            Arc::clone(&registry),
        );
        app.add_systems(Update, sys);

        app.update();

        assert_eq!(
            NESTED_INTERACTION_COUNT.load(Ordering::SeqCst),
            10 * 5,
            "10 drones and 5 players must generate exactly 50 pairwise interactions"
        );
    }
}

// ==============================================================================
// TEST 8: Cross-Language Struct Memory Layout Binary Matching
// ==============================================================================

#[test]
fn test_stress_cross_language_interop_binary_layout() {
    // 1. Position: struct { float x, y, z; }
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

    // 2. Velocity: struct { float x, y, z; }
    assert_eq!(core::mem::size_of::<Velocity>(), 12);
    assert_eq!(core::mem::align_of::<Velocity>(), 4);

    // 3. CombatHealth: struct { int32_t current, max; }
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

    // 4. ColorRgba: struct { float r, g, b, a; }
    assert_eq!(core::mem::size_of::<ColorRgba>(), 16);
    assert_eq!(core::mem::align_of::<ColorRgba>(), 4);
    let color_sample = ColorRgba {
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

    // 5. Byte marshalling fidelity check: Write raw C bytes, read Rust struct
    let raw_c_bytes: [u8; 12] = [
        0x00, 0x00, 0x80, 0x3F, // 1.0f32
        0x00, 0x00, 0x00, 0x40, // 2.0f32
        0x00, 0x00, 0x40, 0x40, // 3.0f32
    ];
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

// ==============================================================================
// TEST 9: Cross-Language Concurrent Runtime Stress (Example 3 C99 + Rust mods)
// ==============================================================================

fn get_example_3_dylibs() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = manifest_dir.parent().unwrap().parent().unwrap();

    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let c_mod_dir = root.join("examples/03_cross_language_interop/c_mod");
    let c_dylib = c_mod_dir.join(format!("{prefix}c_mod.{ext}"));
    if !c_dylib.exists() {
        let status = if cfg!(target_os = "windows") {
            Command::new("gcc")
                .args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-I"])
                .arg(root.join("include"))
                .args(["-O3", "-shared"])
                .arg(c_mod_dir.join("mod.c"))
                .arg("-o")
                .arg(&c_dylib)
                .status()
                .expect("Failed to build Example 3 C mod with gcc")
        } else {
            Command::new("make")
                .arg("-C")
                .arg(&c_mod_dir)
                .status()
                .expect("Failed to build Example 3 C mod")
        };
        assert!(status.success());
    }

    let rust_manifest = root.join("examples/03_cross_language_interop/rust_mod/Cargo.toml");
    let rust_dylib = root
        .join("examples/03_cross_language_interop/rust_mod/target/release")
        .join(format!("{prefix}rust_mod.{ext}"));
    if !rust_dylib.exists() {
        let status = Command::new("cargo")
            .args(["build", "--manifest-path"])
            .arg(&rust_manifest)
            .arg("--release")
            .status()
            .expect("Failed to build Example 3 Rust mod");
        assert!(status.success());
    }

    (c_dylib, rust_dylib)
}

#[test]
fn test_stress_example_3_concurrency_and_lifecycle() {
    let (c_dylib, rust_dylib) = get_example_3_dylibs();

    let mut app = App::new();
    app.add_plugins(AbiModPlugin::default());
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<ColorRgba>();

    // Spawn 500 test drone entities
    const DRONE_COUNT: usize = 500;
    let mut drones = Vec::with_capacity(DRONE_COUNT);
    for i in 0..DRONE_COUNT {
        let e = app
            .world_mut()
            .spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 10.0,
                    y: 5.0,
                    z: 0.0,
                },
                ColorRgba {
                    r: 0.0,
                    g: 0.0,
                    b: 0.0,
                    a: 1.0,
                },
            ))
            .id();
        drones.push(e);
    }

    // 1. Load both C99 and Rust mods
    let c_mod_id = app
        .load_mod_tracked(&c_dylib)
        .expect("Failed to load C99 mod");
    let rust_mod_id = app
        .load_mod_tracked(&rust_dylib)
        .expect("Failed to load Rust mod");

    // 2. Run 50 concurrent ticks
    for _ in 0..50 {
        app.update();
    }

    // Assert cooperative mutations:
    // C99 increments Position by Vel * 0.1 each tick: dx = 50 * (10.0 * 0.1) = 50.0
    // Rust increments ColorRgba based on Position
    let p0 = *app.world().get::<Position>(drones[0]).unwrap();
    let c0 = *app.world().get::<ColorRgba>(drones[0]).unwrap();

    assert!(
        (p0.x - 50.0).abs() < 1e-3,
        "C99 mod must have moved Drone 0 to x ~ 50.0"
    );
    assert!(c0.r > 0.0, "Rust mod must have altered Drone 0 color");
    assert!(!c0.r.is_nan() && !c0.r.is_infinite());

    // 3. Dynamic lifecycle: Unload C99 mod, keep Rust mod running
    app.unload_mod(c_mod_id).expect("Failed to unload C99 mod");

    let p0_snapshot = *app.world().get::<Position>(drones[0]).unwrap();
    let c0_snapshot = *app.world().get::<ColorRgba>(drones[0]).unwrap();

    // Run 25 ticks with only Rust mod active (no host mutations: position remains static)
    for _ in 0..25 {
        app.update();
    }

    let p0_after_c_unload = *app.world().get::<Position>(drones[0]).unwrap();
    assert_eq!(
        p0_snapshot, p0_after_c_unload,
        "Position must be frozen after C99 mod is unloaded"
    );

    // Host mutates Position: verify Rust mod is still active and recomputes ColorRgba
    app.world_mut().get_mut::<Position>(drones[0]).unwrap().x = 2.0;
    app.update();

    let c0_updated = *app.world().get::<ColorRgba>(drones[0]).unwrap();
    // new_r = ((2.0 * 0.05).abs() + 0.1).min(1.0) = 0.2
    assert!(
        (c0_updated.r - 0.2).abs() < 1e-3,
        "Rust mod must dynamically update color to 0.2 when host shifts position to 2.0"
    );
    assert_ne!(
        c0_snapshot.r, c0_updated.r,
        "Color must have changed in response to host position update"
    );

    // 4. Unload Rust mod
    app.unload_mod(rust_mod_id)
        .expect("Failed to unload Rust mod");

    // Mutate position in host again: with Rust mod unloaded, ColorRgba must NOT change!
    app.world_mut().get_mut::<Position>(drones[0]).unwrap().x = 15.0;
    app.update();

    let c0_final = *app.world().get::<ColorRgba>(drones[0]).unwrap();
    assert_eq!(
        c0_final, c0_updated,
        "Color must remain completely frozen after Rust mod is unloaded (atomic tombstone)"
    );
}
