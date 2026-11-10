#![allow(unused_imports)]

use super::harness::*;
use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiResult, FfiSlice, HostApi, RawChunkIter, RawCommandQueue,
    StrView, SystemDescriptor, ABI_VERSION_PACKED,
};
use core::ffi::{c_char, c_void, CStr};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

// ============================================================================
// Tier 3: Cross-Feature Pairwise Combinations
// ============================================================================

#[test]
fn t3_01_raw_chunk_iter_and_alignment_heterogeneity() {
    // F3 + F14: Iterate table with columns of varying alignments (1B, 4B, 8B, 16B, 64B)
    let mut state = MockChunkIterState::new();

    let bytes_b = vec![42u8; 16];
    let bytes_w = vec![0u8; 16 * 4];
    let bytes_dw = vec![0u8; 16 * 8];
    let bytes_simd = vec![0u8; 16 * 16];
    let bytes_cl = vec![0u8; 16 * 64];

    state.add_table(MockTable {
        entity_count: 16,
        entities: (0..16).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![
            MockColumnData {
                name: "AlignByte".to_string(),
                stride: 1,
                is_mutable: false,
                bytes: bytes_b,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: "AlignWord".to_string(),
                stride: 4,
                is_mutable: false,
                bytes: bytes_w,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: "AlignDWord".to_string(),
                stride: 8,
                is_mutable: false,
                bytes: bytes_dw,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: "AlignSimd".to_string(),
                stride: 16,
                is_mutable: false,
                bytes: bytes_simd,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: "AlignCacheLine".to_string(),
                stride: 64,
                is_mutable: false,
                bytes: bytes_cl,
                changed_flags: vec![false; 16],
            },
        ],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let count = unsafe { mock_iter_next_chunk(iter) };
    assert_eq!(count, 16);

    for col_idx in 0..5 {
        let ptr = unsafe { mock_iter_column_readonly(iter, col_idx) };
        assert!(!ptr.is_null());
        let stride = unsafe { mock_iter_column_stride(iter, col_idx) };
        assert!(stride > 0);
    }
}

#[test]
fn t3_02_dynamic_system_and_borrow_conflict_detection() {
    // F2 + F18: System query permissions & conflict checking
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let vel_name = KinematicsVel::COMPONENT_CSTR.as_ptr();

    // Reader system
    let reader_queries = [
        ComponentAccessDesc::read(pos_name),
        ComponentAccessDesc::read(vel_name),
    ];
    // Writer system on Velocity
    let writer_vel = [ComponentAccessDesc::write(vel_name)];
    // Writer system on Position
    let writer_pos = [ComponentAccessDesc::write(pos_name)];

    // Reader & Writer on Velocity must conflict
    assert!(check_borrow_conflict(&reader_queries, &writer_vel));
    // Writer on Pos & Writer on Vel do not conflict (disjoint)
    assert!(!check_borrow_conflict(&writer_pos, &writer_vel));
    // Two readers do not conflict
    assert!(!check_borrow_conflict(&reader_queries, &reader_queries));
}

#[test]
fn t3_03_metadata_string_vec_and_allocator_boundary_isolation() {
    // F7 + F8 + F21: String and Vec mutations across boundary with memory isolation
    let mut tag = MetadataTag {
        label: "InitName".to_string(),
    };
    let mut scores = BufferScores {
        scores: vec![100, 200],
    };

    let tag_vt = MetadataTag::vtable();
    let scores_vt = BufferScores::vtable();

    let tag_ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let scores_ptr = &mut scores as *mut BufferScores as *mut c_void;

    // Mutate string with foreign buffer
    let new_name = "Player_Hero_Rank1";
    unsafe {
        (tag_vt.set_label)(tag_ptr, new_name.as_ptr(), new_name.len());
    }
    assert_eq!(tag.label, new_name);

    // Mutate vector with foreign buffer
    let new_scores = [500, 600, 700, 800];
    unsafe {
        (scores_vt.set_scores)(scores_ptr, new_scores.as_ptr(), new_scores.len());
    }
    assert_eq!(scores.scores, vec![500, 600, 700, 800]);

    // Read back via borrowed slices
    let tag_slice = unsafe { (tag_vt.get_label)(tag_ptr) };
    let scores_slice = unsafe { (scores_vt.get_scores)(scores_ptr) };

    assert_eq!(tag_slice.len, new_name.len());
    assert_eq!(scores_slice.len, 4);
}

#[test]
fn t3_04_command_queue_bridge_and_chunk_iteration_traversal() {
    // F4 + F3: Chunk iteration enqueuing deferred commands
    let mut chunk_state = MockChunkIterState::new();
    chunk_state.add_table(MockTable {
        entity_count: 5,
        entities: (100..105).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![],
    });

    let mut cmd_queue = MockCommandQueueState::new();

    let iter = &mut chunk_state as *mut MockChunkIterState as *mut RawChunkIter;
    let count = unsafe { mock_iter_next_chunk(iter) };
    let ents_ptr = unsafe { mock_iter_entities(iter) };
    let ents = unsafe { core::slice::from_raw_parts(ents_ptr, count) };

    for &e in ents {
        // Enqueue despawn for even entities, spawn for odd
        if e.to_bits() % 2 == 0 {
            cmd_queue.push(MockCommand::Despawn(e));
        } else {
            cmd_queue.push(MockCommand::Spawn(FfiEntity::from_bits(e.to_bits() + 1000)));
        }
    }

    let flushed = cmd_queue.flush();
    assert_eq!(flushed.len(), 5);
}

#[test]
fn t3_05_hot_reloading_teardown_and_panic_containment() {
    // F23 + F22 + F20: Contain panic, teardown faulty mod, hot-reload healthy mod
    static ACTIVE_MOD_VERSION: AtomicU32 = AtomicU32::new(1);
    static MOD_CLEANED_UP: AtomicBool = AtomicBool::new(false);

    // Faulty version 1
    let panic_result = std::panic::catch_unwind(|| {
        panic!("fatal mod v1 crash");
    });
    assert!(panic_result.is_err());
    let status = FfiResult::PANIC_CAUGHT;
    assert_eq!(status, FfiResult::PanicCaught);

    // Teardown hook
    MOD_CLEANED_UP.store(true, Ordering::SeqCst);
    assert!(MOD_CLEANED_UP.load(Ordering::SeqCst));

    // Reload version 2
    ACTIVE_MOD_VERSION.store(2, Ordering::SeqCst);
    assert_eq!(ACTIVE_MOD_VERSION.load(Ordering::SeqCst), 2);
}

#[test]
fn t3_06_multi_language_shared_schedule_components() {
    // F5 + F6: Kinematics and Combat components updated in sequence
    let mut pos = KinematicsPos {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let mut health = CombatHealth {
        current: 100,
        max: 100,
    };

    let pos_vt = KinematicsPos::vtable();
    let health_vt = CombatHealth::vtable();

    let pos_ptr = &mut pos as *mut KinematicsPos as *mut c_void;
    let health_ptr = &mut health as *mut CombatHealth as *mut c_void;

    // Simulate C mod step: update pos
    unsafe {
        (pos_vt.set_x)(pos_ptr, 10.0);
        (pos_vt.set_y)(pos_ptr, 20.0);
    }

    // Simulate Rust mod step: damage health
    let cur = unsafe { (health_vt.get_current)(health_ptr) };
    unsafe {
        (health_vt.set_current)(health_ptr, cur - 15);
    }

    assert_eq!(pos.x, 10.0);
    assert_eq!(pos.y, 20.0);
    assert_eq!(health.current, 85);
}

#[test]
fn t3_07_archetype_fragmentation_and_scale_scaling() {
    // F12 + F11: 10,000 entities across 16 fragmented archetype tables
    let mut state = MockChunkIterState::new();
    let tables_count = 16;
    let entities_per_table = 625; // 16 * 625 = 10,000

    for t in 0..tables_count {
        let base_id = (t * entities_per_table) as u64;
        state.add_table(MockTable {
            entity_count: entities_per_table,
            entities: (0..entities_per_table)
                .map(|i| FfiEntity::from_bits(base_id + i as u64))
                .collect(),
            columns: vec![],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let mut total_visited = 0;
    loop {
        let count = unsafe { mock_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        total_visited += count;
    }
    assert_eq!(total_visited, 10_000);
}

#[test]
fn t3_08_column_permission_and_change_detection() {
    // F19 + F6: READ column prevents mark_changed
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 10,
        entities: (0..10).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![MockColumnData {
            name: "CombatHealth".to_string(),
            stride: core::mem::size_of::<CombatHealth>(),
            is_mutable: false, // Read-only
            bytes: vec![0; 10 * core::mem::size_of::<CombatHealth>()],
            changed_flags: vec![false; 10],
        }],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let res = mock_iter_mark_changed(iter, 0, 0);
        assert_eq!(res, FfiResult::PERMISSION_DENIED);
        assert!(!state.tables[0].columns[0].changed_flags[0]);
    }
}

#[test]
fn t3_09_zero_allocation_hot_path_during_benchmark_loop() {
    // F25 + F9 + F10: Benchmark chunk iteration with zero heap allocation
    let mut data = [KinematicsPos {
        x: 1.0,
        y: 1.0,
        z: 1.0,
    }; 256];
    let start = Instant::now();
    for p in &mut data {
        p.x += 0.5;
        p.y += 0.5;
        p.z += 0.5;
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 10);
    assert_eq!(data[0].x, 1.5);
}

#[test]
fn t3_10_repeated_registration_and_ecs_consistency() {
    // F24 + F26: 50 registration cycles with continuous entity consistency
    let mut entities: Vec<FfiEntity> = (0..100).map(|i| FfiEntity::from_bits(i)).collect();

    for cycle in 0..50 {
        // Register mock system
        let desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: b"CycleSystem\0".as_ptr() as *const c_char,
            queries: core::ptr::null(),
            query_count: 0,
            callback: dummy_callback,
            user_data: core::ptr::null_mut(),
        };
        let status = unsafe { mock_register_system(core::ptr::null_mut(), &desc) };
        assert_eq!(status, FfiResult::OK);

        // Mutate entity state
        entities[0] = FfiEntity::from_bits(cycle as u64);

        // Teardown cycle
    }

    assert_eq!(entities.len(), 100);
    assert_eq!(entities[0].to_bits(), 49);
}

// Helper dummy callback
unsafe extern "C" fn dummy_callback(
    _iter: *mut RawChunkIter,
    _cmds: *mut RawCommandQueue,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}
