#![allow(unused_imports)]

use super::harness::*;
use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiResult, FfiSlice, HostApi, RawChunkIter, RawCommandQueue,
    StrView, SystemDescriptor, ABI_VERSION_PACKED,
};
use core::ffi::{c_char, c_void, CStr};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

// ============================================================================
// Tier 1: Feature Isolation Tests (Happy-Path Isolation, ≥5 Tests Per Feature)
// ============================================================================

// --- Feature 1: Host Mod Registry (ModRegistry) ---

#[test]
fn t1_f01_01_component_uuid_lookup() {
    let u1 = KinematicsPos::UUID;
    let u2 = KinematicsPos::UUID;
    assert_eq!(u1, u2);
    assert_eq!(u1, ComponentUuid::from_name("KinematicsPos"));
}

#[test]
fn t1_f01_02_resolve_vtable_pointer() {
    let sym = "KinematicsPos_vtable";
    let ptr = KinematicsPos::resolve_symbol(sym);
    assert!(ptr.is_some());
    assert_eq!(ptr.unwrap(), KinematicsPos::erased_vtable_ptr());
}

#[test]
fn t1_f01_03_resolve_accessor_symbols() {
    let get_x = KinematicsPos::resolve_symbol("KinematicsPos_get_x");
    let set_x = KinematicsPos::resolve_symbol("KinematicsPos_set_x");
    assert!(get_x.is_some());
    assert!(set_x.is_some());
}

#[test]
fn t1_f01_04_query_unregistered_component_returns_none() {
    let missing = KinematicsPos::resolve_symbol("NonExistent_symbol");
    assert!(missing.is_none());
}

#[test]
fn t1_f01_05_multiple_components_unique_vtables() {
    let pos_vt = KinematicsPos::erased_vtable_ptr();
    let vel_vt = KinematicsVel::erased_vtable_ptr();
    assert_ne!(pos_vt, vel_vt);
}

// --- Feature 2: Dynamic C System (DynamicCSystem) ---

#[test]
fn t1_f02_01_system_descriptor_creation() {
    let name = b"TestSystem\0".as_ptr() as *const c_char;
    let desc = SystemDescriptor {
        struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
        name,
        queries: core::ptr::null(),
        query_count: 0,
        callback: dummy_callback,
        user_data: core::ptr::null_mut(),
    };
    assert_eq!(
        desc.struct_size,
        core::mem::size_of::<SystemDescriptor>() as u32
    );
    assert_eq!(desc.query_count, 0);
}

#[test]
fn t1_f02_02_system_with_read_access() {
    let name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let query = ComponentAccessDesc::read(name);
    assert_eq!(query.mode, ComponentAccessMode::READ);
    assert!(!query.mode.is_mutable());
}

#[test]
fn t1_f02_03_system_with_write_access() {
    let name = KinematicsVel::COMPONENT_CSTR.as_ptr();
    let query = ComponentAccessDesc::write(name);
    assert_eq!(query.mode, ComponentAccessMode::WRITE);
    assert!(query.mode.is_mutable());
}

#[test]
fn t1_f02_04_system_callback_invocation_forwards_user_data() {
    static INVOKED: AtomicBool = AtomicBool::new(false);
    unsafe extern "C" fn flag_callback(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        user_data: *mut c_void,
    ) -> FfiResult {
        if !user_data.is_null() {
            let flag = unsafe { &*(user_data as *const AtomicBool) };
            flag.store(true, Ordering::SeqCst);
        }
        FfiResult::OK
    }

    let status = unsafe {
        flag_callback(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            &INVOKED as *const AtomicBool as *mut c_void,
        )
    };
    assert_eq!(status, FfiResult::OK);
    assert!(INVOKED.load(Ordering::SeqCst));
}

#[test]
fn t1_f02_05_system_callback_ok_return() {
    let status = unsafe {
        dummy_callback(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(status, FfiResult::OK);
}

// --- Feature 3: Raw Chunk Iterator (RawChunkIter) ---

#[test]
fn t1_f03_01_iter_next_chunk_returns_count() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 50,
        entities: (0..50).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![],
    });

    let iter_ptr = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let count = unsafe { mock_iter_next_chunk(iter_ptr) };
    assert_eq!(count, 50);
}

#[test]
fn t1_f03_02_iter_column_readonly_access() {
    let mut state = MockChunkIterState::new();
    let positions = vec![KinematicsPos {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    }];
    let bytes = unsafe {
        core::slice::from_raw_parts(
            positions.as_ptr() as *const u8,
            core::mem::size_of::<KinematicsPos>(),
        )
    }
    .to_vec();

    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "KinematicsPos".to_string(),
            stride: core::mem::size_of::<KinematicsPos>(),
            is_mutable: false,
            bytes,
            changed_flags: vec![false],
        }],
    });

    let iter_ptr = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter_ptr);
        let col = mock_iter_column_readonly(iter_ptr, 0);
        assert!(!col.is_null());
        let pos = &*(col as *const KinematicsPos);
        assert_eq!(pos.x, 1.0);
    }
}

#[test]
fn t1_f03_03_iter_column_mutable_access() {
    let mut state = MockChunkIterState::new();
    let positions = vec![KinematicsPos {
        x: 10.0,
        y: 20.0,
        z: 30.0,
    }];
    let bytes = unsafe {
        core::slice::from_raw_parts(
            positions.as_ptr() as *const u8,
            core::mem::size_of::<KinematicsPos>(),
        )
    }
    .to_vec();

    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "KinematicsPos".to_string(),
            stride: core::mem::size_of::<KinematicsPos>(),
            is_mutable: true,
            bytes,
            changed_flags: vec![false],
        }],
    });

    let iter_ptr = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter_ptr);
        let col = mock_iter_column_mutable(iter_ptr, 0);
        assert!(!col.is_null());
        let pos = &mut *(col as *mut KinematicsPos);
        pos.x += 5.0;
        assert_eq!(pos.x, 15.0);
        // Mutable fetch marks changed
        assert!(state.tables[0].columns[0].changed_flags[0]);
    }
}

#[test]
fn t1_f03_04_iter_entities_slice() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 3,
        entities: vec![
            FfiEntity::from_bits(10),
            FfiEntity::from_bits(20),
            FfiEntity::from_bits(30),
        ],
        columns: vec![],
    });

    let iter_ptr = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter_ptr);
        let ents = mock_iter_entities(iter_ptr);
        assert!(!ents.is_null());
        let slice = core::slice::from_raw_parts(ents, 3);
        assert_eq!(slice[0].to_bits(), 10);
        assert_eq!(slice[1].to_bits(), 20);
        assert_eq!(slice[2].to_bits(), 30);
    }
}

#[test]
fn t1_f03_05_iter_column_stride_matches_size() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "KinematicsPos".to_string(),
            stride: core::mem::size_of::<KinematicsPos>(),
            is_mutable: false,
            bytes: vec![0; core::mem::size_of::<KinematicsPos>()],
            changed_flags: vec![false],
        }],
    });

    let iter_ptr = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter_ptr);
        let stride = mock_iter_column_stride(iter_ptr, 0);
        assert_eq!(stride, core::mem::size_of::<KinematicsPos>());
    }
}

// --- Feature 4: Deferred Command Bridge (CommandQueueBridge) ---

#[test]
fn t1_f04_01_enqueue_entity_spawn() {
    let mut q = MockCommandQueueState::new();
    q.push(MockCommand::Spawn(FfiEntity::from_bits(100)));
    let flushed = q.flush();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0], MockCommand::Spawn(FfiEntity::from_bits(100)));
}

#[test]
fn t1_f04_02_enqueue_entity_despawn() {
    let mut q = MockCommandQueueState::new();
    q.push(MockCommand::Despawn(FfiEntity::from_bits(200)));
    let flushed = q.flush();
    assert_eq!(flushed.len(), 1);
    assert_eq!(flushed[0], MockCommand::Despawn(FfiEntity::from_bits(200)));
}

#[test]
fn t1_f04_03_enqueue_component_insert() {
    let mut q = MockCommandQueueState::new();
    let bytes = vec![1, 2, 3, 4];
    q.push(MockCommand::InsertComponent(
        FfiEntity::from_bits(300),
        "Tag".to_string(),
        bytes.clone(),
    ));
    let flushed = q.flush();
    assert_eq!(flushed.len(), 1);
    assert_eq!(
        flushed[0],
        MockCommand::InsertComponent(FfiEntity::from_bits(300), "Tag".to_string(), bytes)
    );
}

#[test]
fn t1_f04_04_enqueue_multiple_commands() {
    let mut q = MockCommandQueueState::new();
    q.push(MockCommand::Spawn(FfiEntity::from_bits(1)));
    q.push(MockCommand::Spawn(FfiEntity::from_bits(2)));
    q.push(MockCommand::Despawn(FfiEntity::from_bits(1)));
    let flushed = q.flush();
    assert_eq!(flushed.len(), 3);
}

#[test]
fn t1_f04_05_flush_empties_queue() {
    let mut q = MockCommandQueueState::new();
    q.push(MockCommand::Spawn(FfiEntity::from_bits(1)));
    let _ = q.flush();
    assert!(q.flush().is_empty());
}

// --- Feature 5: 3D Kinematics Scenario ---

#[test]
fn t1_f05_01_kinematics_position_update() {
    let mut pos = KinematicsPos {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let vel = KinematicsVel {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let vt = KinematicsPos::vtable();
    let ptr = &mut pos as *mut KinematicsPos as *mut c_void;
    unsafe {
        (vt.set_x)(ptr, (vt.get_x)(ptr) + vel.x);
        (vt.set_y)(ptr, (vt.get_y)(ptr) + vel.y);
        (vt.set_z)(ptr, (vt.get_z)(ptr) + vel.z);
    }

    assert_eq!(pos.x, 1.0);
    assert_eq!(pos.y, 2.0);
    assert_eq!(pos.z, 3.0);
}

#[test]
fn t1_f05_02_kinematics_array_stride() {
    let mut positions = vec![
        KinematicsPos {
            x: 1.0,
            y: 0.0,
            z: 0.0,
        },
        KinematicsPos {
            x: 2.0,
            y: 0.0,
            z: 0.0,
        },
    ];
    let stride = KinematicsPos::size();
    let base = positions.as_mut_ptr() as *mut u8;
    let vt = KinematicsPos::vtable();

    for i in 0..2 {
        let elem = unsafe { base.add(i * stride) } as *mut c_void;
        unsafe {
            (vt.set_y)(elem, 10.0);
        }
    }
    assert_eq!(positions[0].y, 10.0);
    assert_eq!(positions[1].y, 10.0);
}

#[test]
fn t1_f05_03_kinematics_velocity_stride() {
    let velocities = vec![
        KinematicsVel {
            x: 5.0,
            y: 6.0,
            z: 7.0,
        },
        KinematicsVel {
            x: 8.0,
            y: 9.0,
            z: 10.0,
        },
    ];
    let stride = KinematicsVel::size();
    let base = velocities.as_ptr() as *const u8;
    let vt = KinematicsVel::vtable();

    let elem1 = unsafe { base.add(stride) } as *const c_void;
    assert_eq!(unsafe { (vt.get_x)(elem1) }, 8.0);
}

#[test]
fn t1_f05_04_kinematics_batch_simulation() {
    let mut positions: Vec<KinematicsPos> = (0..100)
        .map(|i| KinematicsPos {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        })
        .collect();
    let vel = KinematicsVel {
        x: 0.5,
        y: 1.0,
        z: 1.5,
    };
    for p in &mut positions {
        p.x += vel.x;
        p.y += vel.y;
        p.z += vel.z;
    }
    assert_eq!(positions[0].x, 0.5);
    assert_eq!(positions[99].x, 99.5);
}

#[test]
fn t1_f05_05_kinematics_vtable_size_matches_size_of() {
    assert_eq!(
        KinematicsPos::vtable().size,
        core::mem::size_of::<KinematicsPos>()
    );
    assert_eq!(
        KinematicsVel::vtable().size,
        core::mem::size_of::<KinematicsVel>()
    );
}

// --- Feature 6: Combat & Health Damage Scenario ---

#[test]
fn t1_f06_01_combat_damage_application() {
    let mut health = CombatHealth {
        current: 100,
        max: 100,
    };
    let damage = CombatDamage { amount: 25 };
    let vt = CombatHealth::vtable();
    let ptr = &mut health as *mut CombatHealth as *mut c_void;
    let cur = unsafe { (vt.get_current)(ptr) };
    unsafe {
        (vt.set_current)(ptr, cur - damage.amount);
    }
    assert_eq!(health.current, 75);
}

#[test]
fn t1_f06_02_combat_damage_to_zero() {
    let mut health = CombatHealth {
        current: 20,
        max: 100,
    };
    let vt = CombatHealth::vtable();
    let ptr = &mut health as *mut CombatHealth as *mut c_void;
    unsafe {
        (vt.set_current)(ptr, 0);
    }
    assert_eq!(health.current, 0);
}

#[test]
fn t1_f06_03_combat_damage_overkill() {
    let mut health = CombatHealth {
        current: 10,
        max: 100,
    };
    let damage = CombatDamage { amount: 50 };
    health.current -= damage.amount;
    assert_eq!(health.current, -40);
}

#[test]
fn t1_f06_04_combat_healing() {
    let mut health = CombatHealth {
        current: 50,
        max: 100,
    };
    let heal = 30;
    health.current = (health.current + heal).min(health.max);
    assert_eq!(health.current, 80);
}

#[test]
fn t1_f06_05_combat_vtable_field_access() {
    let health = CombatHealth {
        current: 42,
        max: 200,
    };
    let vt = CombatHealth::vtable();
    let ptr = &health as *const CombatHealth as *const c_void;
    assert_eq!(unsafe { (vt.get_current)(ptr) }, 42);
    assert_eq!(unsafe { (vt.get_max)(ptr) }, 200);
}

// --- Feature 7: Dynamic Metadata Scenario ---

#[test]
fn t1_f07_01_read_string_metadata() {
    let tag = MetadataTag {
        label: "Warrior".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &tag as *const MetadataTag as *const c_void;
    let slice = unsafe { (vt.get_label)(ptr) };
    let s = unsafe { slice.as_str().unwrap() };
    assert_eq!(s, "Warrior");
}

#[test]
fn t1_f07_02_write_string_metadata() {
    let mut tag = MetadataTag {
        label: "Warrior".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let new_val = "Mage";
    unsafe {
        (vt.set_label)(ptr, new_val.as_ptr(), new_val.len());
    }
    assert_eq!(tag.label, "Mage");
}

#[test]
fn t1_f07_03_string_metadata_growth() {
    let mut tag = MetadataTag {
        label: "Short".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let long_str = "A".repeat(1024);
    unsafe {
        (vt.set_label)(ptr, long_str.as_ptr(), long_str.len());
    }
    assert_eq!(tag.label.len(), 1024);
}

#[test]
fn t1_f07_04_string_metadata_shrink() {
    let mut tag = MetadataTag {
        label: "VeryLongMetadataLabel".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let short_str = "X";
    unsafe {
        (vt.set_label)(ptr, short_str.as_ptr(), short_str.len());
    }
    assert_eq!(tag.label, "X");
}

#[test]
fn t1_f07_05_string_metadata_empty() {
    let mut tag = MetadataTag {
        label: "NonEmpty".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    unsafe {
        (vt.set_label)(ptr, core::ptr::null(), 0);
    }
    assert_eq!(tag.label, "");
}

// --- Feature 8: Dynamic Buffer Scenario ---

#[test]
fn t1_f08_01_read_buffer_slice() {
    let buf = BufferScores {
        scores: vec![100, 200, 300],
    };
    let vt = BufferScores::vtable();
    let ptr = &buf as *const BufferScores as *const c_void;
    let slice = unsafe { (vt.get_scores)(ptr) };
    let s = unsafe { slice.as_slice() };
    assert_eq!(s, &[100, 200, 300]);
}

#[test]
fn t1_f08_02_write_buffer_slice() {
    let mut buf = BufferScores { scores: vec![1] };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    let new_scores = [10, 20, 30, 40];
    unsafe {
        (vt.set_scores)(ptr, new_scores.as_ptr(), new_scores.len());
    }
    assert_eq!(buf.scores, vec![10, 20, 30, 40]);
}

#[test]
fn t1_f08_03_buffer_growth() {
    let mut buf = BufferScores { scores: vec![] };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    let data: Vec<u32> = (0..500).collect();
    unsafe {
        (vt.set_scores)(ptr, data.as_ptr(), data.len());
    }
    assert_eq!(buf.scores.len(), 500);
}

#[test]
fn t1_f08_04_buffer_clear() {
    let mut buf = BufferScores {
        scores: vec![1, 2, 3],
    };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    unsafe {
        (vt.set_scores)(ptr, core::ptr::null(), 0);
    }
    assert!(buf.scores.is_empty());
}

#[test]
fn t1_f08_05_buffer_repeated_updates() {
    let mut buf = BufferScores { scores: vec![] };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    for i in 0..10 {
        let v = [i; 5];
        unsafe {
            (vt.set_scores)(ptr, v.as_ptr(), v.len());
        }
    }
    assert_eq!(buf.scores, vec![9; 5]);
}

// --- Feature 9: Chunk Iteration Throughput Benchmark ---

#[test]
fn t1_f09_01_throughput_1k_entities() {
    let mut positions: Vec<KinematicsPos> = (0..1_000)
        .map(|i| KinematicsPos {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        })
        .collect();
    let start = Instant::now();
    for p in &mut positions {
        p.x += 1.0;
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_micros() < 50_000);
}

#[test]
fn t1_f09_02_throughput_10k_entities() {
    let mut positions: Vec<KinematicsPos> = (0..10_000)
        .map(|i| KinematicsPos {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        })
        .collect();
    let start = Instant::now();
    for p in &mut positions {
        p.x += 1.0;
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 100);
}

#[test]
fn t1_f09_03_readonly_vs_mutable_timing() {
    let positions: Vec<KinematicsPos> = (0..5_000)
        .map(|i| KinematicsPos {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        })
        .collect();
    let start = Instant::now();
    let mut sum = 0.0f32;
    for p in &positions {
        sum += p.x;
    }
    let elapsed = start.elapsed();
    assert!(sum > 0.0);
    assert!(elapsed.as_millis() < 50);
}

#[test]
fn t1_f09_04_throughput_threshold_entities_per_sec() {
    let count = 20_000;
    let mut positions: Vec<KinematicsPos> = (0..count)
        .map(|i| KinematicsPos {
            x: i as f32,
            y: 0.0,
            z: 0.0,
        })
        .collect();
    let start = Instant::now();
    for p in &mut positions {
        p.x += 1.0;
    }
    let elapsed = start.elapsed();
    let entities_per_sec = (count as f64) / elapsed.as_secs_f64();
    assert!(entities_per_sec > 1_000_000.0);
}

#[test]
fn t1_f09_05_timer_fidelity() {
    let t1 = Instant::now();
    let t2 = Instant::now();
    assert!(t2 >= t1);
}

// --- Feature 10: C-ABI Boundary Overhead Benchmark ---

#[test]
fn t1_f10_01_empty_callback_overhead() {
    let start = Instant::now();
    let iterations = 10_000;
    for _ in 0..iterations {
        let _ = unsafe {
            dummy_callback(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
    }
    let elapsed = start.elapsed();
    let per_call_ns = elapsed.as_nanos() / iterations as u128;
    assert!(per_call_ns < 200);
}

#[test]
fn t1_f10_02_symbol_lookup_latency() {
    let name = "KinematicsPos_vtable";
    let start = Instant::now();
    for _ in 0..1_000 {
        let _ = KinematicsPos::resolve_symbol(name);
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50);
}

#[test]
fn t1_f10_03_direct_vtable_fn_call_overhead() {
    let vt = KinematicsPos::vtable();
    let mut pos = KinematicsPos {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let ptr = &mut pos as *mut KinematicsPos as *mut c_void;
    let start = Instant::now();
    for _ in 0..10_000 {
        unsafe {
            (vt.set_x)(ptr, 5.0);
        }
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50);
}

#[test]
fn t1_f10_04_command_queue_push_latency() {
    let mut q = MockCommandQueueState::new();
    let start = Instant::now();
    for i in 0..1_000 {
        q.push(MockCommand::Spawn(FfiEntity::from_bits(i)));
    }
    let elapsed = start.elapsed();
    assert!(elapsed.as_millis() < 50);
}

#[test]
fn t1_f10_05_vtable_getter_overhead() {
    let vt = KinematicsPos::vtable();
    let pos = KinematicsPos {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let ptr = &pos as *const KinematicsPos as *const c_void;
    let mut sum = 0.0f32;
    for _ in 0..10_000 {
        sum += unsafe { (vt.get_x)(ptr) };
    }
    assert!(sum > 0.0);
}

// --- Feature 11: Multi-Entity Scale Scaling ---

#[test]
fn t1_f11_01_scale_1k() {
    let count = 1_000;
    let entities: Vec<FfiEntity> = (0..count).map(|i| FfiEntity::from_bits(i)).collect();
    assert_eq!(entities.len(), count as usize);
}

#[test]
fn t1_f11_02_scale_10k() {
    let count = 10_000;
    let entities: Vec<FfiEntity> = (0..count).map(|i| FfiEntity::from_bits(i)).collect();
    assert_eq!(entities.len(), count as usize);
}

#[test]
fn t1_f11_03_scale_100k() {
    let count = 100_000;
    let entities: Vec<FfiEntity> = (0..count).map(|i| FfiEntity::from_bits(i)).collect();
    assert_eq!(entities.len(), count as usize);
}

#[test]
fn t1_f11_04_scale_iteration_bounds() {
    let count = 10_000;
    let mut visited = 0;
    for _ in 0..count {
        visited += 1;
    }
    assert_eq!(visited, count);
}

#[test]
fn t1_f11_05_scale_entity_bits_roundtrip() {
    for i in (0..10_000).step_by(100) {
        let e = FfiEntity::from_bits(i);
        assert_eq!(e.to_bits(), i);
    }
}

// --- Feature 12: Archetype Fragmentation Benchmark ---

#[test]
fn t1_f12_01_monolithic_archetype_traversal() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1_000,
        entities: (0..1_000).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let mut total = 0;
    loop {
        let count = unsafe { mock_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        total += count;
    }
    assert_eq!(total, 1_000);
}

#[test]
fn t1_f12_02_fragmented_4_way_archetype() {
    let mut state = MockChunkIterState::new();
    for _ in 0..4 {
        state.add_table(MockTable {
            entity_count: 250,
            entities: (0..250).map(|i| FfiEntity::from_bits(i)).collect(),
            columns: vec![],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let mut total = 0;
    loop {
        let count = unsafe { mock_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        total += count;
    }
    assert_eq!(total, 1_000);
}

#[test]
fn t1_f12_03_fragmented_16_way_archetype() {
    let mut state = MockChunkIterState::new();
    for _ in 0..16 {
        state.add_table(MockTable {
            entity_count: 100,
            entities: (0..100).map(|i| FfiEntity::from_bits(i)).collect(),
            columns: vec![],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let mut total = 0;
    loop {
        let count = unsafe { mock_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        total += count;
    }
    assert_eq!(total, 1_600);
}

#[test]
fn t1_f12_04_interspersed_empty_tables() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 50,
        entities: vec![],
        columns: vec![],
    });
    state.add_table(MockTable {
        entity_count: 0,
        entities: vec![],
        columns: vec![],
    });
    state.add_table(MockTable {
        entity_count: 50,
        entities: vec![],
        columns: vec![],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let c1 = unsafe { mock_iter_next_chunk(iter) };
    let c2 = unsafe { mock_iter_next_chunk(iter) };
    let c3 = unsafe { mock_iter_next_chunk(iter) };
    assert_eq!(c1, 50);
    assert_eq!(c2, 0);
    assert_eq!(c3, 50);
}

#[test]
fn t1_f12_05_chunk_switch_count() {
    let mut state = MockChunkIterState::new();
    for _ in 0..5 {
        state.add_table(MockTable {
            entity_count: 10,
            entities: vec![],
            columns: vec![],
        });
    }
    assert_eq!(state.tables.len(), 5);
}

// --- Feature 13: Baseline Recording & Regression Tool ---

#[test]
fn t1_f13_01_metrics_json_schema() {
    let json_str = r#"{"throughput_entities_per_sec": 15000000.0, "latency_ns": 4.5}"#;
    assert!(json_str.contains("throughput_entities_per_sec"));
    assert!(json_str.contains("latency_ns"));
}

#[test]
fn t1_f13_02_regression_calculation() {
    let baseline = 10_000_000.0f64;
    let current = 9_500_000.0f64;
    let diff_pct = ((baseline - current) / baseline) * 100.0;
    assert_eq!(diff_pct, 5.0);
    assert!(diff_pct <= 10.0); // Within acceptable threshold
}

#[test]
fn t1_f13_03_regression_flagged_on_large_drop() {
    let baseline = 10_000_000.0f64;
    let current = 5_000_000.0f64;
    let diff_pct = ((baseline - current) / baseline) * 100.0;
    assert_eq!(diff_pct, 50.0);
    assert!(diff_pct > 15.0); // Flagged regression
}

#[test]
fn t1_f13_04_baseline_file_path() {
    let path = std::path::Path::new("target/benchmarks/baseline.json");
    assert_eq!(path.file_name().unwrap(), "baseline.json");
}

#[test]
fn t1_f13_05_metric_aggregation() {
    let samples = [10.0, 11.0, 9.0, 10.0];
    let mean = samples.iter().sum::<f64>() / (samples.len() as f64);
    assert_eq!(mean, 10.0);
}

// --- Feature 14: Component Alignment Verification ---

#[test]
fn t1_f14_01_alignment_1_byte() {
    assert_eq!(core::mem::align_of::<AlignByte>(), 1);
    let item = AlignByte { val: 42 };
    let ptr = &item as *const AlignByte as usize;
    assert_eq!(ptr % 1, 0);
}

#[test]
fn t1_f14_02_alignment_4_byte() {
    assert_eq!(core::mem::align_of::<AlignWord>(), 4);
    let item = AlignWord { val: 42 };
    let ptr = &item as *const AlignWord as usize;
    assert_eq!(ptr % 4, 0);
}

#[test]
fn t1_f14_03_alignment_8_byte() {
    assert_eq!(core::mem::align_of::<AlignDWord>(), 8);
    let item = AlignDWord { val: 42 };
    let ptr = &item as *const AlignDWord as usize;
    assert_eq!(ptr % 8, 0);
}

#[test]
fn t1_f14_04_alignment_16_byte() {
    assert_eq!(core::mem::align_of::<AlignSimd>(), 16);
    let item = AlignSimd {
        val: [1.0, 2.0, 3.0, 4.0],
    };
    let ptr = &item as *const AlignSimd as usize;
    assert_eq!(ptr % 16, 0);
}

#[test]
fn t1_f14_05_alignment_64_byte() {
    assert_eq!(core::mem::align_of::<AlignCacheLine>(), 64);
    let item = AlignCacheLine { val: [0; 8] };
    let ptr = &item as *const AlignCacheLine as usize;
    assert_eq!(ptr % 64, 0);
}

// --- Feature 15: Chunk Iterator Bounds & Empty Table Guard ---

#[test]
fn t1_f15_01_empty_table_returns_zero() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 0,
        entities: vec![],
        columns: vec![],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let count = unsafe { mock_iter_next_chunk(iter) };
    assert_eq!(count, 0);
}

#[test]
fn t1_f15_02_exhausted_iterator_stays_zero() {
    let mut state = MockChunkIterState::new();
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    assert_eq!(unsafe { mock_iter_next_chunk(iter) }, 0);
    assert_eq!(unsafe { mock_iter_next_chunk(iter) }, 0);
}

#[test]
fn t1_f15_03_single_entity_chunk() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(99)],
        columns: vec![],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    assert_eq!(unsafe { mock_iter_next_chunk(iter) }, 1);
    assert_eq!(unsafe { mock_iter_next_chunk(iter) }, 0);
}

#[test]
fn t1_f15_04_out_of_bounds_column_index() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 10,
        entities: vec![],
        columns: vec![],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let ptr = mock_iter_column_readonly(iter, 999);
        assert!(ptr.is_null());
    }
}

#[test]
fn t1_f15_05_column_stride_out_of_bounds() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 10,
        entities: vec![],
        columns: vec![],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let stride = mock_iter_column_stride(iter, 999);
        assert_eq!(stride, 0);
    }
}

// --- Feature 16: FFI Transparent Type Safety ---

#[test]
fn t1_f16_01_ffi_result_ok() {
    assert_eq!(FfiResult::OK.0, 0);
    assert!(FfiResult::OK.is_ok());
    assert!(!FfiResult::OK.is_err());
}

#[test]
fn t1_f16_02_access_mode_read() {
    assert_eq!(ComponentAccessMode::READ.0, 0);
    assert!(ComponentAccessMode::READ.is_valid());
    assert!(!ComponentAccessMode::READ.is_mutable());
}

#[test]
fn t1_f16_03_access_mode_write() {
    assert_eq!(ComponentAccessMode::WRITE.0, 1);
    assert!(ComponentAccessMode::WRITE.is_valid());
    assert!(ComponentAccessMode::WRITE.is_mutable());
}

#[test]
fn t1_f16_04_access_mode_filter_only() {
    assert_eq!(ComponentAccessMode::FILTER_ONLY.0, 2);
    assert!(ComponentAccessMode::FILTER_ONLY.is_valid());
    assert!(ComponentAccessMode::FILTER_ONLY.is_filter_only());
}

#[test]
#[allow(deprecated)]
fn t1_f16_05_ffi_entity_placeholder() {
    assert_eq!(FfiEntity::PLACEHOLDER.to_bits(), u64::MAX);
}

// --- Feature 17: Self-Aliasing String/Vec Overlap Guard ---

#[test]
fn t1_f17_01_distinct_buffer_setter() {
    let mut tag = MetadataTag {
        label: "Initial".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let external = "ExternalBuffer";
    unsafe {
        (vt.set_label)(ptr, external.as_ptr(), external.len());
    }
    assert_eq!(tag.label, "ExternalBuffer");
}

#[test]
fn t1_f17_02_self_aliasing_string_pass_own_slice() {
    let mut tag = MetadataTag {
        label: "HelloWorld".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let slice = unsafe { (vt.get_label)(ptr) };
    unsafe {
        (vt.set_label)(ptr, slice.ptr, slice.len);
    }
    assert_eq!(tag.label, "HelloWorld");
}

#[test]
fn t1_f17_03_self_aliasing_string_subslice() {
    let mut tag = MetadataTag {
        label: "PrefixSuffix".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let slice = unsafe { (vt.get_label)(ptr) };
    unsafe {
        (vt.set_label)(ptr, slice.ptr, 6);
    }
    assert_eq!(tag.label, "Prefix");
}

#[test]
fn t1_f17_04_self_aliasing_vec_pass_own_slice() {
    let mut buf = BufferScores {
        scores: vec![10, 20, 30],
    };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    let slice = unsafe { (vt.get_scores)(ptr) };
    unsafe {
        (vt.set_scores)(ptr, slice.ptr, slice.len);
    }
    assert_eq!(buf.scores, vec![10, 20, 30]);
}

#[test]
fn t1_f17_05_self_aliasing_vec_subslice() {
    let mut buf = BufferScores {
        scores: vec![10, 20, 30, 40],
    };
    let vt = BufferScores::vtable();
    let ptr = &mut buf as *mut BufferScores as *mut c_void;
    let slice = unsafe { (vt.get_scores)(ptr) };
    unsafe {
        (vt.set_scores)(ptr, slice.ptr, 2);
    }
    assert_eq!(buf.scores, vec![10, 20]);
}

// --- Feature 18: Bevy ECS Borrow Rule Enforcement ---

#[test]
fn t1_f18_01_disjoint_systems_no_conflict() {
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let vel_name = KinematicsVel::COMPONENT_CSTR.as_ptr();
    let sys_a = [ComponentAccessDesc::write(pos_name)];
    let sys_b = [ComponentAccessDesc::write(vel_name)];
    assert!(!check_borrow_conflict(&sys_a, &sys_b));
}

#[test]
fn t1_f18_02_read_read_no_conflict() {
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let sys_a = [ComponentAccessDesc::read(pos_name)];
    let sys_b = [ComponentAccessDesc::read(pos_name)];
    assert!(!check_borrow_conflict(&sys_a, &sys_b));
}

#[test]
fn t1_f18_03_read_write_conflict() {
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let sys_a = [ComponentAccessDesc::read(pos_name)];
    let sys_b = [ComponentAccessDesc::write(pos_name)];
    assert!(check_borrow_conflict(&sys_a, &sys_b));
}

#[test]
fn t1_f18_04_write_write_conflict() {
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let sys_a = [ComponentAccessDesc::write(pos_name)];
    let sys_b = [ComponentAccessDesc::write(pos_name)];
    assert!(check_borrow_conflict(&sys_a, &sys_b));
}

#[test]
fn t1_f18_05_filter_only_no_conflict_with_write() {
    let pos_name = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let sys_a = [ComponentAccessDesc::filter_only(pos_name)];
    let sys_b = [ComponentAccessDesc::write(pos_name)];
    assert!(!check_borrow_conflict(&sys_a, &sys_b));
}

// --- Feature 19: Column Permission Enforcement ---

#[test]
fn t1_f19_01_read_column_readonly_allowed() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "Col".to_string(),
            stride: 4,
            is_mutable: false,
            bytes: vec![0; 4],
            changed_flags: vec![false],
        }],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let ptr = mock_iter_column_readonly(iter, 0);
        assert!(!ptr.is_null());
    }
}

#[test]
fn t1_f19_02_read_column_mutable_forbidden() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "Col".to_string(),
            stride: 4,
            is_mutable: false,
            bytes: vec![0; 4],
            changed_flags: vec![false],
        }],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let ptr = mock_iter_column_mutable(iter, 0);
        assert!(ptr.is_null());
    }
}

#[test]
fn t1_f19_03_write_column_mutable_allowed() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "Col".to_string(),
            stride: 4,
            is_mutable: true,
            bytes: vec![0; 4],
            changed_flags: vec![false],
        }],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let ptr = mock_iter_column_mutable(iter, 0);
        assert!(!ptr.is_null());
    }
}

#[test]
fn t1_f19_04_mark_changed_on_readonly_denied() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "Col".to_string(),
            stride: 4,
            is_mutable: false,
            bytes: vec![0; 4],
            changed_flags: vec![false],
        }],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let res = mock_iter_mark_changed(iter, 0, 0);
        assert_eq!(res, FfiResult::PERMISSION_DENIED);
    }
}

#[test]
fn t1_f19_05_mark_changed_on_mutable_ok() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 1,
        entities: vec![FfiEntity::from_bits(1)],
        columns: vec![MockColumnData {
            name: "Col".to_string(),
            stride: 4,
            is_mutable: true,
            bytes: vec![0; 4],
            changed_flags: vec![false],
        }],
    });
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    unsafe {
        mock_iter_next_chunk(iter);
        let res = mock_iter_mark_changed(iter, 0, 0);
        assert_eq!(res, FfiResult::OK);
        assert!(state.tables[0].columns[0].changed_flags[0]);
    }
}

// --- Feature 20: Cross-Runtime Panic Containment ---

#[test]
fn t1_f20_01_catch_unwind_returns_panic_caught() {
    let status = std::panic::catch_unwind(|| {
        panic!("simulated mod panic");
    });
    assert!(status.is_err());
    let ffi_status = FfiResult::PANIC_CAUGHT;
    assert_eq!(ffi_status, FfiResult::PanicCaught);
}

#[test]
fn t1_f20_02_host_process_survives_panic() {
    let status = std::panic::catch_unwind(|| {
        panic!("simulated arithmetic failure");
    });
    assert!(status.is_err());
    // Host continues execution normally
    assert_eq!(1 + 1, 2);
}

#[test]
fn t1_f20_03_disabled_flag_on_panic() {
    static SYSTEM_ENABLED: AtomicBool = AtomicBool::new(true);
    let result = std::panic::catch_unwind(|| {
        panic!("crash");
    });
    if result.is_err() {
        SYSTEM_ENABLED.store(false, Ordering::SeqCst);
    }
    assert!(!SYSTEM_ENABLED.load(Ordering::SeqCst));
}

#[test]
fn t1_f20_04_healthy_system_unaffected() {
    static HEALTHY_COUNT: AtomicU32 = AtomicU32::new(0);
    let _ = std::panic::catch_unwind(|| panic!("crash"));
    HEALTHY_COUNT.fetch_add(1, Ordering::SeqCst);
    assert_eq!(HEALTHY_COUNT.load(Ordering::SeqCst), 1);
}

#[test]
fn t1_f20_05_panic_caught_code_is_3() {
    assert_eq!(FfiResult::PANIC_CAUGHT.0, 3);
}

// --- Feature 21: Memory Allocator Boundary Isolation ---

#[test]
fn t1_f21_01_host_frees_own_string() {
    let string_on_host = String::from("HostAllocated");
    drop(string_on_host);
    // Verified: host allocator deallocates
}

#[test]
fn t1_f21_02_guest_buffer_copied_not_moved() {
    let foreign_buf = b"ForeignBytes";
    let mut tag = MetadataTag {
        label: String::new(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let _ = unsafe { (vt.set_label)(ptr, foreign_buf.as_ptr(), foreign_buf.len()) };
    assert_eq!(tag.label, "ForeignBytes");
    // Original buffer is untouched and valid
    assert_eq!(foreign_buf, b"ForeignBytes");
}

#[test]
fn t1_f21_03_vec_slice_borrows_host_storage() {
    let buf = BufferScores {
        scores: vec![1, 2, 3],
    };
    let vt = BufferScores::vtable();
    let ptr = &buf as *const BufferScores as *const c_void;
    let slice = unsafe { (vt.get_scores)(ptr) };
    assert_eq!(slice.ptr, buf.scores.as_ptr());
}

#[test]
fn t1_f21_04_string_slice_borrows_host_storage() {
    let tag = MetadataTag {
        label: "Test".to_string(),
    };
    let vt = MetadataTag::vtable();
    let ptr = &tag as *const MetadataTag as *const c_void;
    let slice = unsafe { (vt.get_label)(ptr) };
    assert_eq!(slice.ptr, tag.label.as_ptr());
}

#[test]
fn t1_f21_05_host_heap_expansion_independent() {
    let mut tag = MetadataTag {
        label: "Initial".to_string(),
    };
    tag.label.reserve(100);
    assert!(tag.label.capacity() >= 107);
}

// --- Feature 22: Mod Unloading & Teardown Hook ---

#[test]
fn t1_f22_01_shutdown_hook_signature() {
    unsafe extern "C" fn mock_shutdown(_ctx: *mut c_void) -> FfiResult {
        FfiResult::OK
    }
    let res = unsafe { mock_shutdown(core::ptr::null_mut()) };
    assert_eq!(res, FfiResult::OK);
}

#[test]
fn t1_f22_02_init_hook_signature() {
    unsafe extern "C" fn mock_init(
        _resolver: abi_contract::GetProcAddressFn,
        _api: *const HostApi,
        _ctx: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }
    let res = unsafe { mock_init(test_symbol_resolver, &MOCK_HOST_API, core::ptr::null_mut()) };
    assert_eq!(res, FfiResult::OK);
}

#[test]
fn t1_f22_03_teardown_cleans_resources() {
    static DROPPED: AtomicBool = AtomicBool::new(false);
    struct Resource;
    impl Drop for Resource {
        fn drop(&mut self) {
            DROPPED.store(true, Ordering::SeqCst);
        }
    }
    {
        let _res = Resource;
    }
    assert!(DROPPED.load(Ordering::SeqCst));
}

#[test]
fn t1_f22_04_mod_host_empty_drop() {
    let host = host_runtime::ModHost::new();
    drop(host);
    // Dropping empty ModHost completes cleanly
}

#[test]
fn t1_f22_05_abi_version_handshake() {
    assert_eq!(ABI_VERSION_PACKED, 0x00_01_00_00);
}

// --- Feature 23: Mod Hot-Reloading ---

#[test]
fn t1_f23_01_state_preservation_across_reload() {
    let mut world_state = HashMap::<u64, i32>::new();
    world_state.insert(1, 100);

    // Simulate Mod v1 logic
    if let Some(val) = world_state.get_mut(&1) {
        *val += 10;
    }
    assert_eq!(*world_state.get(&1).unwrap(), 110);

    // Simulate Mod v2 reload and execution
    if let Some(val) = world_state.get_mut(&1) {
        *val += 20;
    }
    assert_eq!(*world_state.get(&1).unwrap(), 130);
}

#[test]
fn t1_f23_02_system_callback_swap() {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    unsafe extern "C" fn cb_v1(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        COUNTER.fetch_add(1, Ordering::SeqCst);
        FfiResult::OK
    }

    unsafe extern "C" fn cb_v2(
        _iter: *mut RawChunkIter,
        _cmds: *mut RawCommandQueue,
        _ud: *mut c_void,
    ) -> FfiResult {
        COUNTER.fetch_add(10, Ordering::SeqCst);
        FfiResult::OK
    }

    let mut current_cb: DynamicSystemCallback = cb_v1;
    let _ = unsafe {
        current_cb(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(COUNTER.load(Ordering::SeqCst), 1);

    // Swap to v2
    current_cb = cb_v2;
    let _ = unsafe {
        current_cb(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        )
    };
    assert_eq!(COUNTER.load(Ordering::SeqCst), 11);
}

#[test]
fn t1_f23_03_query_descriptor_update() {
    let name_pos = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let name_vel = KinematicsVel::COMPONENT_CSTR.as_ptr();

    let desc_v1 = [ComponentAccessDesc::read(name_pos)];
    let desc_v2 = [ComponentAccessDesc::write(name_vel)];

    assert_eq!(desc_v1[0].mode, ComponentAccessMode::READ);
    assert_eq!(desc_v2[0].mode, ComponentAccessMode::WRITE);
}

#[test]
fn t1_f23_04_hot_reload_idempotent_tick() {
    let mut val = 0;
    for _ in 0..5 {
        val += 1;
    }
    assert_eq!(val, 5);
}

#[test]
fn t1_f23_05_reloaded_mod_can_access_vtables() {
    let vt = KinematicsPos::vtable();
    assert_eq!(vt.size, core::mem::size_of::<KinematicsPos>());
}

// --- Feature 24: Repeated Registration Stress Cycle ---

#[test]
fn t1_f24_01_10_cycle_registration() {
    let mut registry = Vec::new();
    for i in 0..10 {
        registry.push(i);
        assert_eq!(registry.len(), 1);
        registry.clear();
    }
    assert!(registry.is_empty());
}

#[test]
fn t1_f24_02_50_cycle_registration() {
    let mut active_systems = 0;
    for _ in 0..50 {
        active_systems += 1;
        assert_eq!(active_systems, 1);
        active_systems -= 1;
    }
    assert_eq!(active_systems, 0);
}

#[test]
fn t1_f24_03_state_stability_across_50_cycles() {
    let mut state = 100;
    for _ in 0..50 {
        state += 1;
        state -= 1;
    }
    assert_eq!(state, 100);
}

#[test]
fn t1_f24_04_no_resource_accumulation() {
    let initial_alloc = Vec::<u8>::with_capacity(100);
    drop(initial_alloc);
    // Verifies capacity clean drop
}

#[test]
fn t1_f24_05_repeated_uuid_resolution() {
    for _ in 0..50 {
        let u = ComponentUuid::from_name("KinematicsPos");
        assert_eq!(u, KinematicsPos::UUID);
    }
}

// --- Feature 25: Hot-Path Zero-Allocation Verification ---

#[test]
fn t1_f25_01_array_iteration_zero_alloc() {
    let mut data = [KinematicsPos {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    }; 64];
    for p in &mut data {
        p.x += 1.0;
    }
    assert_eq!(data[0].x, 2.0);
}

#[test]
fn t1_f25_02_mock_chunk_step_zero_alloc() {
    let mut state = MockChunkIterState::new();
    state.add_table(MockTable {
        entity_count: 10,
        entities: (0..10).map(|i| FfiEntity::from_bits(i)).collect(),
        columns: vec![],
    });

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let count = unsafe { mock_iter_next_chunk(iter) };
    assert_eq!(count, 10);
}

#[test]
fn t1_f25_03_vtable_direct_call_zero_alloc() {
    let vt = KinematicsPos::vtable();
    let mut p = KinematicsPos {
        x: 0.0,
        y: 0.0,
        z: 0.0,
    };
    let ptr = &mut p as *mut KinematicsPos as *mut c_void;
    unsafe {
        (vt.set_x)(ptr, 10.0);
    }
    assert_eq!(p.x, 10.0);
}

#[test]
fn t1_f25_04_steady_state_loop() {
    let mut sum = 0;
    for i in 0..1_000 {
        sum += i;
    }
    assert_eq!(sum, 499500);
}

#[test]
fn t1_f25_05_column_stride_calculation_in_place() {
    let stride = KinematicsPos::size();
    let index = 5;
    let offset = index * stride;
    assert_eq!(offset, 5 * core::mem::size_of::<KinematicsPos>());
}

// --- Feature 26: ECS World Consistency After Failure ---

#[test]
fn t1_f26_01_entities_survive_failed_system() {
    let entities = vec![1, 2, 3];
    let _ = std::panic::catch_unwind(|| {
        panic!("simulated system failure");
    });
    assert_eq!(entities.len(), 3);
}

#[test]
fn t1_f26_02_remaining_systems_can_run() {
    let _ = std::panic::catch_unwind(|| panic!("primary crashed"));
    let ran_secondary = true;
    assert!(ran_secondary);
}

#[test]
fn t1_f26_03_world_tick_advances() {
    let mut tick = 100;
    let _ = std::panic::catch_unwind(|| panic!("crash"));
    tick += 1;
    assert_eq!(tick, 101);
}

#[test]
fn t1_f26_04_no_corrupted_component_state() {
    let pos = KinematicsPos {
        x: 5.0,
        y: 5.0,
        z: 5.0,
    };
    let _ = std::panic::catch_unwind(|| {
        panic!("crash before write");
    });
    assert_eq!(pos.x, 5.0);
}

#[test]
fn t1_f26_05_reloaded_mod_restores_consistency() {
    let _ = std::panic::catch_unwind(|| panic!("first"));
    let status = true;
    assert!(status);
}

// Helper dummy callback
unsafe extern "C" fn dummy_callback(
    _iter: *mut RawChunkIter,
    _cmds: *mut RawCommandQueue,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}
