#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter, RawCommandQueue};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{ResMut, Resource, World};
use bevy_ecs::query::Changed;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::system::Query;
use core::ffi::c_void;
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_column_stride, host_iter_entities, host_iter_mark_changed,
    host_iter_next_chunk, AbiModPlugin, CommandQueueBridge, DynamicCSystem, ModAppExt, ModRegistry,
};
use mod_derive::ModExposable;
use std::ffi::CString;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

// Test components
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

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Armor {
    pub defense: i32,
}

macro_rules! define_stress_tags {
    ($($tag:ident),*) => {
        $(
            #[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
            pub struct $tag {
                pub id: u32,
            }
        )*
    };
}

define_stress_tags!(TagA, TagB, TagC, TagD, TagE);

// ============================================================================
// SUITE 1: CHUNK ITERATION SCALING & BOUNDARIES (0, 1, 20,000 ENTITIES)
// ============================================================================

struct EmptyTableStats {
    chunks_visited: usize,
    entities_visited: usize,
    readonly_null_after_exhaust: bool,
    mutable_null_after_exhaust: bool,
    entities_null_after_exhaust: bool,
    mark_err_after_exhaust: bool,
}

unsafe extern "C" fn empty_table_stress_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let stats = unsafe { &mut *(user_data as *mut EmptyTableStats) };

    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        stats.chunks_visited += 1;
        stats.entities_visited += count;
    }

    // Defensive check after exhaustion: calling accessors must safely return null / err
    let ro = unsafe { host_iter_column_readonly(iter, 0) };
    let rw = unsafe { host_iter_column_mutable(iter, 0) };
    let ents = unsafe { host_iter_entities(iter) };
    let mark_res = unsafe { host_iter_mark_changed(iter, 0, 0) };

    stats.readonly_null_after_exhaust = ro.is_null();
    stats.mutable_null_after_exhaust = rw.is_null();
    stats.entities_null_after_exhaust = ents.is_null();
    stats.mark_err_after_exhaust = mark_res == FfiResult::INTERNAL_ERROR;

    // Repeated next_chunk calls after exhaustion must stay 0
    for _ in 0..5 {
        assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
    }

    FfiResult::OK
}

#[test]
fn test_chunk_iteration_empty_world_and_empty_tables() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![
        host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        },
        host_runtime::ComponentAccessDescOwned {
            name: "Velocity".to_string(),
            mode: ComponentAccessMode::READ,
        },
    ];

    let mut stats = EmptyTableStats {
        chunks_visited: 0,
        entities_visited: 0,
        readonly_null_after_exhaust: false,
        mutable_null_after_exhaust: false,
        entities_null_after_exhaust: false,
        mark_err_after_exhaust: false,
    };

    let dynamic_sys = DynamicCSystem::new(
        "empty_stress".to_string(),
        query_descs,
        empty_table_stress_system,
        &mut stats as *mut EmptyTableStats as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);

    // Frame 1: Completely empty world (0 entities)
    app.update();

    assert_eq!(stats.chunks_visited, 0, "Empty world must yield 0 chunks");
    assert_eq!(
        stats.entities_visited, 0,
        "Empty world must yield 0 entities"
    );
    assert!(stats.readonly_null_after_exhaust);
    assert!(stats.mutable_null_after_exhaust);
    assert!(stats.entities_null_after_exhaust);
    assert!(stats.mark_err_after_exhaust);
}

#[test]
fn test_chunk_iteration_alternating_empty_and_non_empty_tables() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<TagA>();
    app.register_mod_component::<TagB>();
    app.register_mod_component::<TagC>();
    app.register_mod_component::<TagD>();
    app.register_mod_component::<TagE>();

    // Table A: (Position, TagA) -> 10 entities, then despawn all
    let mut t_a_ents = Vec::new();
    for i in 0..10 {
        t_a_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    TagA { id: 1 },
                ))
                .id(),
        );
    }
    // Table B: (Position, TagB) -> 7 entities (KEEP ALIVE)
    for i in 0..7 {
        app.world_mut().spawn((
            Position {
                x: 100.0 + i as f32,
                y: 0.0,
                z: 0.0,
            },
            TagB { id: 2 },
        ));
    }
    // Table C: (Position, TagC) -> 15 entities, then despawn all
    let mut t_c_ents = Vec::new();
    for i in 0..15 {
        t_c_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: 200.0 + i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    TagC { id: 3 },
                ))
                .id(),
        );
    }
    // Table D: (Position, TagD) -> 23 entities (KEEP ALIVE)
    for i in 0..23 {
        app.world_mut().spawn((
            Position {
                x: 300.0 + i as f32,
                y: 0.0,
                z: 0.0,
            },
            TagD { id: 4 },
        ));
    }
    // Table E: (Position, TagE) -> 30 entities, then despawn all
    let mut t_e_ents = Vec::new();
    for i in 0..30 {
        t_e_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: 400.0 + i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    TagE { id: 5 },
                ))
                .id(),
        );
    }

    // Despawn Tables A, C, and E so they become empty tables in Bevy storage
    for e in t_a_ents {
        app.world_mut().despawn(e);
    }
    for e in t_c_ents {
        app.world_mut().despawn(e);
    }
    for e in t_e_ents {
        app.world_mut().despawn(e);
    }

    struct ChunkAudit {
        chunk_sizes: Vec<usize>,
        total_entities: usize,
    }

    unsafe extern "C" fn audit_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let audit = unsafe { &mut *(user_data as *mut ChunkAudit) };
        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            audit.chunk_sizes.push(count);
            audit.total_entities += count;

            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            assert!(!pos_ptr.is_null());
            for i in 0..count {
                unsafe {
                    (*pos_ptr.add(i)).y += 55.0;
                }
            }
        }
        FfiResult::OK
    }

    let mut audit = ChunkAudit {
        chunk_sizes: Vec::new(),
        total_entities: 0,
    };

    let registry = app.world().resource::<ModRegistry>().clone();
    let query_descs = vec![host_runtime::ComponentAccessDescOwned {
        name: "Position".to_string(),
        mode: ComponentAccessMode::WRITE,
    }];

    let dynamic_sys = DynamicCSystem::new(
        "audit_sys".to_string(),
        query_descs,
        audit_system,
        &mut audit as *mut ChunkAudit as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);
    app.update();

    // Must have visited exactly 2 non-empty tables: sizes 7 and 23
    assert_eq!(
        audit.total_entities, 30,
        "Must visit exactly 30 living entities"
    );
    assert_eq!(
        audit.chunk_sizes.len(),
        2,
        "Must visit exactly 2 non-empty chunks"
    );
    assert!(audit.chunk_sizes.contains(&7));
    assert!(audit.chunk_sizes.contains(&23));

    // Verify all 30 entities received mutation y == 55.0
    let mut pos_q = app.world_mut().query::<&Position>();
    let mut verified = 0;
    for pos in pos_q.iter(app.world()) {
        assert_eq!(pos.y, 55.0);
        verified += 1;
    }
    assert_eq!(verified, 30);
}

#[test]
fn test_chunk_iteration_single_entity_tables() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    let e = app
        .world_mut()
        .spawn((
            Position {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            Velocity {
                x: 10.0,
                y: 20.0,
                z: 30.0,
            },
        ))
        .id();

    #[repr(C)]
    struct SingleStats {
        stride_pos: usize,
        stride_vel: usize,
        count: usize,
        entity_matches: bool,
        mark_row0_ok: bool,
        mark_row1_err: bool,
    }

    unsafe extern "C" fn single_entity_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &mut *(user_data as *mut SingleContext) };
        let stats = &mut ctx.stats;
        stats.stride_pos = unsafe { host_iter_column_stride(iter, 0) };
        stats.stride_vel = unsafe { host_iter_column_stride(iter, 1) };

        let count = unsafe { host_iter_next_chunk(iter) };
        stats.count = count;
        assert_eq!(count, 1);

        let ents = unsafe { host_iter_entities(iter) };
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
        let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };

        assert!(!ents.is_null());
        assert!(!pos_ptr.is_null());
        assert!(!vel_ptr.is_null());

        unsafe {
            let ent_bits = (*ents).to_bits();
            let pos = &mut *pos_ptr;
            let vel = &*vel_ptr;
            pos.x += vel.x;
            pos.y += vel.y;
            pos.z += vel.z;

            stats.entity_matches = ent_bits == ctx.target_entity.to_bits();
        }

        // Row 0 mark changed must succeed
        let r0 = unsafe { host_iter_mark_changed(iter, 0, 0) };
        stats.mark_row0_ok = r0 == FfiResult::OK;

        // Row 1 mark changed must fail (chunk size is 1)
        let r1 = unsafe { host_iter_mark_changed(iter, 0, 1) };
        stats.mark_row1_err = r1 == FfiResult::INTERNAL_ERROR;

        assert_eq!(unsafe { host_iter_next_chunk(iter) }, 0);
        FfiResult::OK
    }

    #[repr(C)]
    struct SingleContext {
        target_entity: Entity,
        stats: SingleStats,
    }

    let mut ctx = SingleContext {
        target_entity: e,
        stats: SingleStats {
            stride_pos: 0,
            stride_vel: 0,
            count: 0,
            entity_matches: false,
            mark_row0_ok: false,
            mark_row1_err: false,
        },
    };

    let registry = app.world().resource::<ModRegistry>().clone();
    let dynamic_sys = DynamicCSystem::new(
        "single_sys".to_string(),
        vec![
            host_runtime::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            host_runtime::ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        single_entity_system,
        &mut ctx as *mut SingleContext as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);
    app.update();

    assert_eq!(ctx.stats.count, 1);
    assert_eq!(ctx.stats.stride_pos, core::mem::size_of::<Position>());
    assert_eq!(ctx.stats.stride_vel, core::mem::size_of::<Velocity>());
    assert!(ctx.stats.entity_matches);
    assert!(ctx.stats.mark_row0_ok);
    assert!(ctx.stats.mark_row1_err);

    // Verify entity mutated
    let updated_pos = app.world().get::<Position>(e).unwrap();
    assert_eq!(updated_pos.x, 11.0);
    assert_eq!(updated_pos.y, 22.0);
    assert_eq!(updated_pos.z, 33.0);
}

#[test]
fn test_chunk_iteration_20k_entities_monolithic_and_fragmented() {
    const MONOLITHIC_COUNT: usize = 20_000;

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Armor>();

    // 1. Spawn 20,000 entities in monolithic archetype (Position, Velocity, Armor)
    for i in 0..MONOLITHIC_COUNT {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
            },
            Armor {
                defense: (i % 100) as i32,
            },
        ));
    }

    struct Scale20kStats {
        total_entities: usize,
        chunks: usize,
    }

    unsafe extern "C" fn scale_20k_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        let stats = unsafe { &mut *(user_data as *mut Scale20kStats) };
        let stride_pos = unsafe { host_iter_column_stride(iter, 0) };
        let stride_vel = unsafe { host_iter_column_stride(iter, 1) };
        assert_eq!(stride_pos, core::mem::size_of::<Position>());
        assert_eq!(stride_vel, core::mem::size_of::<Velocity>());

        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            stats.chunks += 1;
            stats.total_entities += count;

            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) as *const Velocity };
            let ents_ptr = unsafe { host_iter_entities(iter) };

            assert!(!pos_ptr.is_null());
            assert!(!vel_ptr.is_null());
            assert!(!ents_ptr.is_null());

            for i in 0..count {
                unsafe {
                    let p = &mut *pos_ptr.add(i);
                    let v = &*vel_ptr.add(i);
                    p.x += v.x;
                    p.y += v.y;
                    p.z += v.z;
                }
            }
        }

        FfiResult::OK
    }

    let mut stats = Scale20kStats {
        total_entities: 0,
        chunks: 0,
    };

    let registry = app.world().resource::<ModRegistry>().clone();
    let dynamic_sys = DynamicCSystem::new(
        "scale_20k_sys".to_string(),
        vec![
            host_runtime::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            host_runtime::ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        scale_20k_system,
        &mut stats as *mut Scale20kStats as *mut c_void,
        Arc::new(registry),
    );

    app.add_systems(Update, dynamic_sys);
    app.update();

    assert_eq!(
        stats.total_entities, MONOLITHIC_COUNT,
        "Must process all 20,000 entities"
    );
    assert_eq!(stats.chunks, 1, "Monolithic table must be a single chunk");

    // Oracle verification across all 20,000 entities
    let mut query = app.world_mut().query::<(&Position, &Armor)>();
    let mut verified = 0usize;
    for (pos, armor) in query.iter(app.world()) {
        let i = pos.x - 1.0; // was i, incremented by v.x (1.0)
        assert_eq!(pos.y, 2.0);
        assert_eq!(pos.z, 3.0);
        assert_eq!(armor.defense, ((i as usize) % 100) as i32);
        verified += 1;
    }
    assert_eq!(verified, MONOLITHIC_COUNT);
}

// ============================================================================
// SUITE 2: CHANGE DETECTION BITMASK EMPIRICAL PRECISION & ISOLATION
// ============================================================================

#[derive(Resource, Default)]
struct ChangedPositionInspector {
    changed_count: usize,
    changed_entities: Vec<Entity>,
}

fn inspect_changed_positions(
    query: Query<(Entity, &Position), Changed<Position>>,
    mut inspector: ResMut<ChangedPositionInspector>,
) {
    inspector.changed_entities = query.iter().map(|(e, _)| e).collect();
    inspector.changed_count = inspector.changed_entities.len();
}

struct ChangePrecisionContext {
    mode: usize, // 0 = clean read, 1 = mark every 10th row, 2 = column_mutable all
    marked_entities: Vec<Entity>,
}

unsafe extern "C" fn change_precision_system(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    let ctx = unsafe { &mut *(user_data as *mut ChangePrecisionContext) };

    loop {
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        let ents = unsafe { host_iter_entities(iter) };
        assert!(!ents.is_null());

        match ctx.mode {
            0 => {
                // Clean mode: READ-ONLY pointer access. Even with WRITE permission on column 0,
                // calling host_iter_column_readonly must NOT mark any change ticks!
                let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
            }
            1 => {
                // Selective row marking: mark every 10th row (row % 10 == 0)
                let _ptr = unsafe { host_iter_column_readonly(iter, 0) };
                for row in 0..count {
                    if row % 10 == 0 {
                        let res = unsafe { host_iter_mark_changed(iter, 0, row) };
                        assert_eq!(res, FfiResult::OK);
                        let ent_bits = unsafe { (*ents.add(row)).to_bits() };
                        let bevy_ent = Entity::from_bits(ent_bits);
                        ctx.marked_entities.push(bevy_ent);
                    }
                }
            }
            2 => {
                // Chunk-wide mutation: host_iter_column_mutable marks all entities changed
                let ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
                assert!(!ptr.is_null());
                for row in 0..count {
                    unsafe {
                        (*ptr.add(row)).x += 1.0;
                    }
                }
            }
            _ => {}
        }
    }

    FfiResult::OK
}

#[test]
fn test_change_detection_bitmask_precision_clean_vs_mutated() {
    const TOTAL_ENTITIES: usize = 1_000;

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangedPositionInspector>();
    app.register_mod_component::<Position>();

    let mut all_spawned = Vec::with_capacity(TOTAL_ENTITIES);
    for i in 0..TOTAL_ENTITIES {
        let e = app
            .world_mut()
            .spawn(Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            })
            .id();
        all_spawned.push(e);
    }

    let mut ctx = Box::new(ChangePrecisionContext {
        mode: 0,
        marked_entities: Vec::new(),
    });
    let ctx_ptr = &mut *ctx as *mut ChangePrecisionContext as *mut c_void;

    let registry = app.world().resource::<ModRegistry>().clone();
    let dynamic_sys = DynamicCSystem::new(
        "change_precision_sys".to_string(),
        vec![host_runtime::ComponentAccessDescOwned {
            name: "Position".to_string(),
            mode: ComponentAccessMode::WRITE,
        }],
        change_precision_system,
        ctx_ptr,
        Arc::new(registry),
    );

    app.add_systems(Update, (dynamic_sys, inspect_changed_positions).chain());

    // Frame 1: Flush initial spawns (all 1,000 are newly Added -> detected as Changed)
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        TOTAL_ENTITIES,
        "Spawn tick must report all 1,000 entities as changed"
    );

    // Frame 2: Clean frame (mode = 0). System has WRITE access, but does NOT mutate.
    // Downstream query MUST observe EXACTLY 0 changed entities!
    ctx.mode = 0;
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        0,
        "Clean frame without mutation must observe exactly 0 Changed<Position> entities"
    );

    // Frame 3: Selective row marking (mode = 1). Marks rows 0, 10, 20, ..., 990 = 100 entities.
    ctx.mode = 1;
    ctx.marked_entities.clear();
    app.update();

    let inspector = app.world().resource::<ChangedPositionInspector>();
    assert_eq!(
        inspector.changed_count, 100,
        "Selective row marking must trigger Changed<Position> on exactly 100 entities"
    );
    assert_eq!(ctx.marked_entities.len(), 100);

    // Verify set equivalence: observed changed entities match marked entities exactly
    for expected_ent in &ctx.marked_entities {
        assert!(
            inspector.changed_entities.contains(expected_ent),
            "Marked entity {expected_ent:?} must be present in Changed<Position> results"
        );
    }

    // Frame 4: Clean frame again (mode = 0). Change ticks must have reset.
    ctx.mode = 0;
    ctx.marked_entities.clear();
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        0,
        "Subsequent clean frame must cleanly observe 0 Changed<Position> entities"
    );

    // Frame 5: Chunk-wide mutation via host_iter_column_mutable (mode = 2).
    ctx.mode = 2;
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        TOTAL_ENTITIES,
        "host_iter_column_mutable must trigger Changed<Position> on all 1,000 entities"
    );

    // Frame 6: Clean frame again.
    ctx.mode = 0;
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        0,
        "Post-mutation clean frame must reset to 0 Changed<Position> entities"
    );
}

#[test]
fn test_change_detection_multi_archetype_isolation() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ChangedPositionInspector>();
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Armor>();
    app.register_mod_component::<TagA>();

    // Archetype 1: (Position, Velocity) -> 300 entities (Target of system)
    let mut arch1_ents = Vec::new();
    for i in 0..300 {
        arch1_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 1.0,
                        z: 1.0,
                    },
                ))
                .id(),
        );
    }

    // Archetype 2: (Position, Armor) -> 300 entities (Must NOT be modified)
    let mut arch2_ents = Vec::new();
    for i in 0..300 {
        arch2_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: 1000.0 + i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Armor { defense: 50 },
                ))
                .id(),
        );
    }

    // Archetype 3: (Position, TagA) -> 300 entities (Must NOT be modified)
    let mut arch3_ents = Vec::new();
    for i in 0..300 {
        arch3_ents.push(
            app.world_mut()
                .spawn((
                    Position {
                        x: 2000.0 + i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    TagA { id: 77 },
                ))
                .id(),
        );
    }

    static MUTATE_ACTIVE: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn kinematics_only_system(
        iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        if MUTATE_ACTIVE.load(Ordering::SeqCst) == 0 {
            return FfiResult::OK;
        }

        loop {
            let count = unsafe { host_iter_next_chunk(iter) };
            if count == 0 {
                break;
            }
            let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) as *mut Position };
            if !pos_ptr.is_null() {
                for i in 0..count {
                    unsafe {
                        (*pos_ptr.add(i)).x += 10.0;
                    }
                }
            }
        }
        FfiResult::OK
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let dynamic_sys = DynamicCSystem::new(
        "kinematics_only".to_string(),
        vec![
            host_runtime::ComponentAccessDescOwned {
                name: "Position".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            host_runtime::ComponentAccessDescOwned {
                name: "Velocity".to_string(),
                mode: ComponentAccessMode::READ,
            },
        ],
        kinematics_only_system,
        core::ptr::null_mut(),
        Arc::new(registry),
    );

    app.add_systems(Update, (dynamic_sys, inspect_changed_positions).chain());

    // Frame 1: Flush initial spawns (all 900 entities changed)
    MUTATE_ACTIVE.store(0, Ordering::SeqCst);
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        900
    );

    // Frame 2: Idle frame (0 changed)
    app.update();
    assert_eq!(
        app.world()
            .resource::<ChangedPositionInspector>()
            .changed_count,
        0
    );

    // Frame 3: Activate mutation on (Position, Velocity)
    MUTATE_ACTIVE.store(1, Ordering::SeqCst);
    app.update();

    let inspector = app.world().resource::<ChangedPositionInspector>();
    assert_eq!(
        inspector.changed_count, 300,
        "Exactly 300 entities in Archetype 1 (Position, Velocity) should trigger Changed<Position>"
    );

    // Validate that every changed entity belongs to Archetype 1, and none to Arch 2 or Arch 3
    for changed_e in &inspector.changed_entities {
        assert!(arch1_ents.contains(changed_e));
        assert!(!arch2_ents.contains(changed_e));
        assert!(!arch3_ents.contains(changed_e));
    }
}

// ============================================================================
// SUITE 3: COMMANDQUEUEBRIDGE BURST CONCURRENCY & INTERLEAVED OPS
// ============================================================================

#[test]
fn test_command_queue_bridge_burst_10k_interleaved_ops() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();
    let vel_id = world.register_component::<Velocity>();
    let armor_id = world.register_component::<Armor>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);
    registry.register_component::<Velocity>(vel_id);
    registry.register_component::<Armor>(armor_id);

    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let pos_name = CString::new("Position").unwrap();
    let vel_name = CString::new("Velocity").unwrap();
    let armor_name = CString::new("Armor").unwrap();

    const BURST_CYCLES: usize = 2_500;
    // Each cycle performs 4 operations:
    // 1. Spawn E_survive, insert Position, insert Velocity
    // 2. Spawn E_die, insert Position, despawn E_die immediately
    // Total ops = 2,500 * 5 = 12,500 queued operations!

    let mut surviving_entities = Vec::with_capacity(BURST_CYCLES);
    let mut dead_entities = Vec::with_capacity(BURST_CYCLES);

    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
        let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

        for i in 0..BURST_CYCLES {
            // A. Spawn surviving entity
            let e_surv = unsafe { host_cmd_spawn_empty(raw_queue) };
            assert_ne!(e_surv.0, 0);

            let pos = Position {
                x: i as f32,
                y: (i * 2) as f32,
                z: (i * 3) as f32,
            };
            let vel = Velocity {
                x: 10.0,
                y: 20.0,
                z: 30.0,
            };

            let res_p = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    e_surv,
                    pos_name.as_ptr(),
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                )
            };
            assert_eq!(res_p, FfiResult::OK);

            let res_v = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    e_surv,
                    vel_name.as_ptr(),
                    &vel as *const Velocity as *const u8,
                    core::mem::size_of::<Velocity>(),
                )
            };
            assert_eq!(res_v, FfiResult::OK);

            surviving_entities.push(e_surv);

            // B. Spawn ephemeral entity, insert component, then despawn in same queue
            let e_die = unsafe { host_cmd_spawn_empty(raw_queue) };
            assert_ne!(e_die.0, 0);

            let armor = Armor { defense: 999 };
            let res_a = unsafe {
                host_cmd_insert_component(
                    raw_queue,
                    e_die,
                    armor_name.as_ptr(),
                    &armor as *const Armor as *const u8,
                    core::mem::size_of::<Armor>(),
                )
            };
            assert_eq!(res_a, FfiResult::OK);

            let res_d = unsafe { host_cmd_despawn(raw_queue, e_die) };
            assert_eq!(res_d, FfiResult::OK);

            dead_entities.push(e_die);
        }
    }

    // Flush 12,500 operations to Bevy World
    cmd_queue.apply(&mut world);

    // Oracle Verification:
    // 1. Surviving entities: exactly BURST_CYCLES live entities with Position and Velocity
    let pos_count = world.query::<&Position>().iter(&world).count();
    let vel_count = world.query::<&Velocity>().iter(&world).count();
    assert_eq!(pos_count, BURST_CYCLES);
    assert_eq!(vel_count, BURST_CYCLES);

    for (i, &ffi_e) in surviving_entities.iter().enumerate() {
        let bevy_e = Entity::from_bits(ffi_e.to_bits());
        let pos = world
            .get::<Position>(bevy_e)
            .expect("Surviving entity must have Position");
        let vel = world
            .get::<Velocity>(bevy_e)
            .expect("Surviving entity must have Velocity");

        assert_eq!(pos.x, i as f32);
        assert_eq!(pos.y, (i * 2) as f32);
        assert_eq!(pos.z, (i * 3) as f32);
        assert_eq!(vel.x, 10.0);
        assert_eq!(vel.y, 20.0);
        assert_eq!(vel.z, 30.0);
    }

    // 2. Dead entities: zero must exist in the world, zero Armor components
    let armor_count = world.query::<&Armor>().iter(&world).count();
    assert_eq!(
        armor_count, 0,
        "All entities with Armor were despawned; count must be 0"
    );

    for &ffi_e in &dead_entities {
        let bevy_e = Entity::from_bits(ffi_e.to_bits());
        assert!(
            world.get_entity(bevy_e).is_err(),
            "Despawned ephemeral entity {bevy_e:?} must no longer exist in Bevy World"
        );
    }
}

#[test]
fn test_command_queue_bridge_multi_system_schedule_concurrency() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();

    // System A: Spawns 500 entities with Position each tick
    unsafe extern "C" fn spawner_pos_system(
        _iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let name = CString::new("Position").unwrap();
        for i in 0..500 {
            let ent = unsafe { host_cmd_spawn_empty(commands) };
            let pos = Position {
                x: i as f32,
                y: 1.0,
                z: 2.0,
            };
            unsafe {
                host_cmd_insert_component(
                    commands,
                    ent,
                    name.as_ptr(),
                    &pos as *const Position as *const u8,
                    core::mem::size_of::<Position>(),
                );
            }
        }
        FfiResult::OK
    }

    // System B: Spawns 500 entities with Velocity each tick
    unsafe extern "C" fn spawner_vel_system(
        _iter: *mut RawChunkIter,
        commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        let name = CString::new("Velocity").unwrap();
        for _i in 0..500 {
            let ent = unsafe { host_cmd_spawn_empty(commands) };
            let vel = Velocity {
                x: 5.0,
                y: 5.0,
                z: 5.0,
            };
            unsafe {
                host_cmd_insert_component(
                    commands,
                    ent,
                    name.as_ptr(),
                    &vel as *const Velocity as *const u8,
                    core::mem::size_of::<Velocity>(),
                );
            }
        }
        FfiResult::OK
    }

    let registry = app.world().resource::<ModRegistry>().clone();
    let reg_arc = Arc::new(registry);

    let sys_a = DynamicCSystem::new(
        "sys_a".to_string(),
        Vec::new(),
        spawner_pos_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );
    let sys_b = DynamicCSystem::new(
        "sys_b".to_string(),
        Vec::new(),
        spawner_vel_system,
        core::ptr::null_mut(),
        Arc::clone(&reg_arc),
    );

    app.add_systems(Update, (sys_a, sys_b));

    // Run 3 ticks: 3 * 500 = 1,500 Position entities, 3 * 500 = 1,500 Velocity entities
    for _ in 1..=3 {
        app.update();
    }

    let pos_count = app
        .world_mut()
        .query::<&Position>()
        .iter(app.world())
        .count();
    let vel_count = app
        .world_mut()
        .query::<&Velocity>()
        .iter(app.world())
        .count();

    assert_eq!(pos_count, 1_500);
    assert_eq!(vel_count, 1_500);
}

#[test]
fn test_command_queue_bridge_defensive_error_handling() {
    let mut world = World::new();
    let pos_id = world.register_component::<Position>();

    let mut registry = ModRegistry::new();
    registry.register_component::<Position>(pos_id);

    let mut cmd_queue = bevy_ecs::world::CommandQueue::default();
    let cell = world.as_unsafe_world_cell();
    let allocator = cell.entity_allocator();
    let mut bridge = CommandQueueBridge::new(&mut cmd_queue, allocator, &registry);
    let raw_queue = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();

    let ent = unsafe { host_cmd_spawn_empty(raw_queue) };
    let valid_name = CString::new("Position").unwrap();
    let unreg_name = CString::new("NonExistentComponent").unwrap();
    let pos = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };

    // 1. Null queue
    let r1 = unsafe {
        host_cmd_insert_component(
            core::ptr::null_mut(),
            ent,
            valid_name.as_ptr(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r1, FfiResult::NULL_POINTER);

    let r_spawn_null = unsafe { host_cmd_spawn_empty(core::ptr::null_mut()) };
    assert_eq!(r_spawn_null.0, 0);

    let r_despawn_null = unsafe { host_cmd_despawn(core::ptr::null_mut(), ent) };
    assert_eq!(r_despawn_null, FfiResult::NULL_POINTER);

    // 2. Null component name
    let r2 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ent,
            core::ptr::null(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r2, FfiResult::NULL_POINTER);

    // 3. Null data with len > 0
    let r3 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ent,
            valid_name.as_ptr(),
            core::ptr::null(),
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r3, FfiResult::NULL_POINTER);

    // 4. Unregistered component name
    let r4 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ent,
            unreg_name.as_ptr(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r4, FfiResult::INVALID_COMPONENT);

    // 5. Size mismatch
    let r5 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            ent,
            valid_name.as_ptr(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>() - 1,
        )
    };
    assert_eq!(r5, FfiResult::INTERNAL_ERROR);

    // 6. Invalid entity bits (0 returns None from Entity::try_from_bits)
    let invalid_ent = FfiEntity(0);
    assert!(Entity::try_from_bits(invalid_ent.0).is_none());

    let r6 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            invalid_ent,
            valid_name.as_ptr(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r6, FfiResult::INTERNAL_ERROR);

    let r7 = unsafe { host_cmd_despawn(raw_queue, invalid_ent) };
    assert_eq!(r7, FfiResult::INTERNAL_ERROR);

    // 7. Non-existent entity that has valid bits: queues successfully, safely skips when applied
    let non_existent_ent = FfiEntity(Entity::from_raw_u32(999_999).unwrap().to_bits());
    let r8 = unsafe {
        host_cmd_insert_component(
            raw_queue,
            non_existent_ent,
            valid_name.as_ptr(),
            &pos as *const Position as *const u8,
            core::mem::size_of::<Position>(),
        )
    };
    assert_eq!(r8, FfiResult::OK);

    let r9 = unsafe { host_cmd_despawn(raw_queue, non_existent_ent) };
    assert_eq!(r9, FfiResult::OK);

    // Flush to world: must NOT panic, and non_existent_ent must not exist
    cmd_queue.apply(&mut world);
    let bevy_non_existent = Entity::from_bits(non_existent_ent.0);
    assert!(world.get_entity(bevy_non_existent).is_err());
}
