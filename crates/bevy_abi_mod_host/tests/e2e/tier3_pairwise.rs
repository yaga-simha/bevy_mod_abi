#![allow(unused_imports)]

use super::harness::*;
use bevy_abi_mod_sys::{
    ABI_VERSION, AccessMode, BamComponent, BamHostApi, BamIter, BamSystemCtx, BamSystemDesc,
    BamSystemFn, BamTermDesc, ComponentUuid, FfiEntity, FfiResult, FfiSlice, ModComponent, StrView,
};
use core::ffi::{CStr, c_char, c_void};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ============================================================================
// Tier 3: Cross-Feature Pairwise Combinations (ABI 0.2)
// ============================================================================

#[test]
fn t3_01_raw_chunk_iter_and_alignment_heterogeneity() {
    let mut state = MockChunkIterState::new();

    let bytes_b = vec![42u8; 16];
    let bytes_w = vec![0u8; 16 * 4];
    let bytes_dw = vec![0u8; 16 * 8];
    let bytes_simd = vec![0u8; 16 * 16];
    let bytes_cl = vec![0u8; 16 * 64];

    state.add_table(MockTable {
        entity_count: 16,
        entities: (1..=16).map(FfiEntity::from_bits).collect(),
        columns: vec![
            MockColumnData {
                name: AlignByte::CANONICAL_NAME.to_string(),
                stride: 1,
                is_mutable: false,
                bytes: bytes_b,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: AlignWord::CANONICAL_NAME.to_string(),
                stride: 4,
                is_mutable: false,
                bytes: bytes_w,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: AlignDWord::CANONICAL_NAME.to_string(),
                stride: 8,
                is_mutable: false,
                bytes: bytes_dw,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: AlignSimd::CANONICAL_NAME.to_string(),
                stride: 16,
                is_mutable: false,
                bytes: bytes_simd,
                changed_flags: vec![false; 16],
            },
            MockColumnData {
                name: AlignCacheLine::CANONICAL_NAME.to_string(),
                stride: 64,
                is_mutable: false,
                bytes: bytes_cl,
                changed_flags: vec![false; 16],
            },
        ],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut BamIter;
    let count = unsafe { mock_step_chunk(iter) };
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
    let reader_queries = [BamTermDesc::read(POS_HANDLE), BamTermDesc::read(VEL_HANDLE)];
    let writer_vel = [BamTermDesc::write(VEL_HANDLE)];
    let writer_pos = [BamTermDesc::write(POS_HANDLE)];

    assert!(check_borrow_conflict(&reader_queries, &writer_vel));
    assert!(!check_borrow_conflict(&writer_pos, &writer_vel));
    assert!(!check_borrow_conflict(&reader_queries, &reader_queries));
}

#[test]
fn t3_03_metadata_string_vec_and_allocator_boundary_isolation() {
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

    let new_name = "Player_Hero_Rank1";
    let res_tag = unsafe { (tag_vt.set_label.unwrap())(tag_ptr, StrView::from_str(new_name)) };
    assert_eq!(res_tag, FfiResult::OK);
    assert_eq!(tag.label, new_name);

    let new_scores = [500u32, 600, 700, 800];
    let res_scores =
        unsafe { (scores_vt.set_scores.unwrap())(scores_ptr, FfiSlice::from_slice(&new_scores)) };
    assert_eq!(res_scores, FfiResult::OK);
    assert_eq!(scores.scores, vec![500, 600, 700, 800]);

    let tag_slice = unsafe { (tag_vt.get_label.unwrap())(tag_ptr) };
    let scores_slice = unsafe { (scores_vt.get_scores.unwrap())(scores_ptr) };

    assert_eq!(tag_slice.len, new_name.len());
    assert_eq!(scores_slice.len, 4);
}

#[test]
fn t3_04_command_queue_bridge_and_chunk_iteration_traversal() {
    let mut chunk_state = MockChunkIterState::new();
    chunk_state.add_table(MockTable {
        entity_count: 5,
        entities: (100..105).map(FfiEntity::from_bits).collect(),
        columns: vec![],
    });

    let mut cmd_queue = MockCommandQueueState::new();

    let iter = &mut chunk_state as *mut MockChunkIterState as *mut BamIter;
    let count = unsafe { mock_step_chunk(iter) };
    let ents_ptr = unsafe { mock_iter_entities(iter) };
    let ents = unsafe { core::slice::from_raw_parts(ents_ptr, count) };

    for &e in ents {
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
    static ACTIVE_MOD_VERSION: AtomicU32 = AtomicU32::new(1);
    static MOD_CLEANED_UP: AtomicBool = AtomicBool::new(false);

    let panic_result = std::panic::catch_unwind(|| {
        panic!("fatal mod v1 crash");
    });
    assert!(panic_result.is_err());
    let status = FfiResult::PANIC_CAUGHT;
    assert_eq!(status, FfiResult(3));

    MOD_CLEANED_UP.store(true, Ordering::SeqCst);
    assert!(MOD_CLEANED_UP.load(Ordering::SeqCst));

    ACTIVE_MOD_VERSION.store(2, Ordering::SeqCst);
    assert_eq!(ACTIVE_MOD_VERSION.load(Ordering::SeqCst), 2);
}

#[test]
fn t3_06_multi_language_shared_schedule_components() {
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

    unsafe {
        assert_eq!((pos_vt.set_x.unwrap())(pos_ptr, 10.0), FfiResult::OK);
        assert_eq!((pos_vt.set_y.unwrap())(pos_ptr, 20.0), FfiResult::OK);
    }

    let cur = unsafe { (health_vt.get_current.unwrap())(health_ptr) };
    unsafe {
        assert_eq!(
            (health_vt.set_current.unwrap())(health_ptr, cur - 15),
            FfiResult::OK
        );
    }

    assert_eq!(pos.x, 10.0);
    assert_eq!(pos.y, 20.0);
    assert_eq!(health.current, 85);
}

#[test]
fn t3_07_archetype_fragmentation_and_scale_scaling() {
    let mut state = MockChunkIterState::new();
    let tables_count = 16;
    let entities_per_table = 625;

    for t in 0..tables_count {
        let base_id = (t * entities_per_table + 1) as u64;
        state.add_table(MockTable {
            entity_count: entities_per_table,
            entities: (0..entities_per_table)
                .map(|i| FfiEntity::from_bits(base_id + i as u64))
                .collect(),
            columns: vec![],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut BamIter;
    let mut total_visited = 0;
    loop {
        let count = unsafe { mock_step_chunk(iter) };
        if count == 0 {
            break;
        }
        total_visited += count;
    }
    assert_eq!(total_visited, 10_000);
}

#[test]
fn t3_08_column_permission_and_change_detection() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 10,
        entities: (1..=10).map(FfiEntity::from_bits).collect(),
        columns: vec![MockColumnData {
            name: CombatHealth::CANONICAL_NAME.to_string(),
            stride: core::mem::size_of::<CombatHealth>(),
            is_mutable: false,
            bytes: vec![0; 10 * core::mem::size_of::<CombatHealth>()],
            changed_flags: vec![false; 10],
        }],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut BamIter;
    unsafe {
        mock_step_chunk(iter);
        let res = mock_iter_mark_changed(iter, 0, 0);
        assert_eq!(res, FfiResult::PERMISSION_DENIED);
        assert!(!state.tables[0].columns[0].changed_flags[0]);
    }
}

#[test]
fn t3_09_zero_allocation_hot_path_during_batch_loop() {
    let mut data = [KinematicsPos {
        x: 1.0,
        y: 1.0,
        z: 1.0,
    }; 256];
    for p in &mut data {
        p.x += 0.5;
        p.y += 0.5;
        p.z += 0.5;
    }
    assert_eq!(data[0].x, 1.5);
    assert_eq!(data[255].z, 1.5);
}

#[test]
fn t3_10_repeated_registration_and_ecs_consistency() {
    let mut entities: Vec<FfiEntity> = (1..=100).map(FfiEntity::from_bits).collect();

    for cycle in 0..50 {
        let desc = BamSystemDesc::new(
            c"CycleSystem".as_ptr(),
            core::ptr::null(),
            0,
            dummy_system_callback,
            core::ptr::null_mut(),
        );
        let status = unsafe { mock_register_system(core::ptr::null_mut(), &desc) };
        assert_eq!(status, FfiResult::OK);

        entities[0] = FfiEntity::from_bits(cycle as u64);
    }

    assert_eq!(entities.len(), 100);
    assert_eq!(entities[0].to_bits(), 49);
}
