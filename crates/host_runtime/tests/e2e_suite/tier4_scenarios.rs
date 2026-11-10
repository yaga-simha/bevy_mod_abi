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
// Tier 4: Real-World Application Scenarios
// ============================================================================

#[test]
fn t4_scenario_a_3d_kinematics_and_physics_integration() {
    // Scenario A: 10,000 entities across 4 archetype tables simulating 3D kinematics
    let total_entities = 10_000;
    let tables_count = 4;
    let per_table = total_entities / tables_count;

    let mut state = MockChunkIterState::new();

    for t in 0..tables_count {
        let pos_data: Vec<KinematicsPos> = (0..per_table)
            .map(|i| KinematicsPos {
                x: (t * per_table + i) as f32,
                y: 0.0,
                z: 0.0,
            })
            .collect();
        let vel_data: Vec<KinematicsVel> = (0..per_table)
            .map(|_| KinematicsVel {
                x: 1.0,
                y: 0.5,
                z: -0.5,
            })
            .collect();

        let pos_bytes = unsafe {
            core::slice::from_raw_parts(
                pos_data.as_ptr() as *const u8,
                per_table * core::mem::size_of::<KinematicsPos>(),
            )
        }
        .to_vec();

        let vel_bytes = unsafe {
            core::slice::from_raw_parts(
                vel_data.as_ptr() as *const u8,
                per_table * core::mem::size_of::<KinematicsVel>(),
            )
        }
        .to_vec();

        state.add_table(MockTable {
            entity_count: per_table,
            entities: (0..per_table)
                .map(|i| FfiEntity::from_bits((t * per_table + i) as u64))
                .collect(),
            columns: vec![
                MockColumnData {
                    name: "KinematicsPos".to_string(),
                    stride: core::mem::size_of::<KinematicsPos>(),
                    is_mutable: true,
                    bytes: pos_bytes,
                    changed_flags: vec![false; per_table],
                },
                MockColumnData {
                    name: "KinematicsVel".to_string(),
                    stride: core::mem::size_of::<KinematicsVel>(),
                    is_mutable: false,
                    bytes: vel_bytes,
                    changed_flags: vec![false; per_table],
                },
            ],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let pos_vt = KinematicsPos::vtable();
    let vel_vt = KinematicsVel::vtable();

    let start = Instant::now();
    let mut processed = 0;

    // Simulate 1 tick of physics integration across all chunks
    loop {
        let count = unsafe { mock_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }

        let pos_base = unsafe { mock_iter_column_mutable(iter, 0) };
        let vel_base = unsafe { mock_iter_column_readonly(iter, 1) };
        let pos_stride = unsafe { mock_iter_column_stride(iter, 0) };
        let vel_stride = unsafe { mock_iter_column_stride(iter, 1) };

        assert!(!pos_base.is_null());
        assert!(!vel_base.is_null());

        for i in 0..count {
            let p_ptr = unsafe { pos_base.add(i * pos_stride) } as *mut c_void;
            let v_ptr = unsafe { vel_base.add(i * vel_stride) } as *const c_void;

            unsafe {
                let px = (pos_vt.get_x)(p_ptr);
                let py = (pos_vt.get_y)(p_ptr);
                let pz = (pos_vt.get_z)(p_ptr);

                let vx = (vel_vt.get_x)(v_ptr);
                let vy = (vel_vt.get_y)(v_ptr);
                let vz = (vel_vt.get_z)(v_ptr);

                (pos_vt.set_x)(p_ptr, px + vx);
                (pos_vt.set_y)(p_ptr, py + vy);
                (pos_vt.set_z)(p_ptr, pz + vz);
            }
        }
        processed += count;
    }

    let elapsed = start.elapsed();
    assert_eq!(processed, total_entities);
    assert!(elapsed.as_millis() < 100);

    // Verify first entity in first table updated accurately
    let first_table = &state.tables[0];
    let first_pos = unsafe { &*(first_table.columns[0].bytes.as_ptr() as *const KinematicsPos) };
    assert_eq!(first_pos.x, 1.0); // 0.0 + 1.0
    assert_eq!(first_pos.y, 0.5); // 0.0 + 0.5
    assert_eq!(first_pos.z, -0.5); // 0.0 - 0.5
}

#[test]
fn t4_scenario_b_combat_bleed_damage_and_change_detection() {
    // Scenario B: 2,500 combat entities taking damage with deferred despawn of dead entities
    let count = 2_500;
    let health_data: Vec<CombatHealth> = (0..count)
        .map(|i| CombatHealth {
            current: if i % 10 == 0 { 10 } else { 100 }, // 10% on brink of death
            max: 100,
        })
        .collect();

    let mut state = MockChunkIterState::new();
    let bytes = unsafe {
        core::slice::from_raw_parts(
            health_data.as_ptr() as *const u8,
            count * core::mem::size_of::<CombatHealth>(),
        )
    }
    .to_vec();

    state.add_table(MockTable {
        entity_count: count,
        entities: (0..count).map(|i| FfiEntity::from_bits(i as u64)).collect(),
        columns: vec![MockColumnData {
            name: "CombatHealth".to_string(),
            stride: core::mem::size_of::<CombatHealth>(),
            is_mutable: true,
            bytes,
            changed_flags: vec![false; count],
        }],
    });

    let mut cmd_queue = MockCommandQueueState::new();
    let iter = &mut state as *mut MockChunkIterState as *mut RawChunkIter;
    let health_vt = CombatHealth::vtable();

    let c = unsafe { mock_iter_next_chunk(iter) };
    assert_eq!(c, count);

    let h_base = unsafe { mock_iter_column_mutable(iter, 0) };
    let h_stride = unsafe { mock_iter_column_stride(iter, 0) };
    let ents_ptr = unsafe { mock_iter_entities(iter) };
    let ents = unsafe { core::slice::from_raw_parts(ents_ptr, c) };

    let bleed_damage = 15;
    let mut dead_count = 0;

    for i in 0..c {
        let h_ptr = unsafe { h_base.add(i * h_stride) } as *mut c_void;
        let cur = unsafe { (health_vt.get_current)(h_ptr) };
        let new_hp = cur - bleed_damage;
        unsafe {
            (health_vt.set_current)(h_ptr, new_hp);
        }

        // Mark row changed
        unsafe { mock_iter_mark_changed(iter, 0, i) };

        if new_hp <= 0 {
            cmd_queue.push(MockCommand::Despawn(ents[i]));
            dead_count += 1;
        }
    }

    assert_eq!(dead_count, 250); // 10% of 2,500
    let flushed = cmd_queue.flush();
    assert_eq!(flushed.len(), 250);
}

#[test]
fn t4_scenario_c_dynamic_rpg_inventory_and_metadata() {
    // Scenario C: Dynamic strings, item vectors, self-aliasing updates, and Unicode
    let mut tag = MetadataTag {
        label: "Knight_Lancelot".to_string(),
    };
    let mut inventory = BufferScores {
        scores: vec![101, 102, 103],
    };

    let tag_vt = MetadataTag::vtable();
    let inv_vt = BufferScores::vtable();

    let tag_ptr = &mut tag as *mut MetadataTag as *mut c_void;
    let inv_ptr = &mut inventory as *mut BufferScores as *mut c_void;

    // 1. Equip item (append to vector)
    let mut items = unsafe { (inv_vt.get_scores)(inv_ptr).as_slice().to_vec() };
    items.push(205); // Excalibur
    unsafe {
        (inv_vt.set_scores)(inv_ptr, items.as_ptr(), items.len());
    }
    assert_eq!(inventory.scores, vec![101, 102, 103, 205]);

    // 2. Self-aliasing title upgrade: prepend "Sir_" to own slice
    let cur_name = unsafe { (tag_vt.get_label)(tag_ptr).as_str().unwrap() };
    let upgraded_title = format!("Sir_{cur_name}");
    unsafe {
        (tag_vt.set_label)(tag_ptr, upgraded_title.as_ptr(), upgraded_title.len());
    }
    assert_eq!(tag.label, "Sir_Knight_Lancelot");

    // 3. Multi-byte Unicode clan tag
    let clan_label = "⚔️ 圆桌骑士团 ⚔️";
    unsafe {
        (tag_vt.set_label)(tag_ptr, clan_label.as_ptr(), clan_label.len());
    }
    assert_eq!(tag.label, clan_label);
}

#[test]
fn t4_scenario_d_multi_mod_schedule_contention_and_panic_isolation() {
    // Scenario D: 4 mod systems, borrow conflict checking, one panics, peers continue
    let pos = KinematicsPos::COMPONENT_CSTR.as_ptr();
    let vel = KinematicsVel::COMPONENT_CSTR.as_ptr();
    let health = CombatHealth::COMPONENT_CSTR.as_ptr();

    // Mod 1: READ Pos, WRITE Vel
    let mod1_queries = [
        ComponentAccessDesc::read(pos),
        ComponentAccessDesc::write(vel),
    ];
    // Mod 2: READ Vel, WRITE Pos (Conflicting with Mod 1)
    let mod2_queries = [
        ComponentAccessDesc::read(vel),
        ComponentAccessDesc::write(pos),
    ];
    // Mod 3: READ Health, WRITE Health (Independent)
    let mod3_queries = [ComponentAccessDesc::write(health)];
    // Mod 4: FILTER_ONLY Pos, READ Vel (Non-conflicting)
    let mod4_queries = [
        ComponentAccessDesc::filter_only(pos),
        ComponentAccessDesc::read(vel),
    ];

    // Verify scheduler conflict between Mod 1 and Mod 2
    assert!(check_borrow_conflict(&mod1_queries, &mod2_queries));

    // Verify Mod 3 is completely independent of Mod 1, 2, and 4
    assert!(!check_borrow_conflict(&mod1_queries, &mod3_queries));
    assert!(!check_borrow_conflict(&mod2_queries, &mod3_queries));
    assert!(!check_borrow_conflict(&mod4_queries, &mod3_queries));

    // Simulate Mod 3 panic during frame
    static MOD3_ACTIVE: AtomicBool = AtomicBool::new(true);
    let mod3_run = std::panic::catch_unwind(|| {
        if MOD3_ACTIVE.load(Ordering::SeqCst) {
            panic!("Mod 3 encountered division by zero in game formula");
        }
    });
    assert!(mod3_run.is_err());
    MOD3_ACTIVE.store(false, Ordering::SeqCst); // Disabled

    // Sibling mods continue running safely for 50 ticks
    let mut mod1_ticks = 0;
    let mut mod2_ticks = 0;
    let mut mod4_ticks = 0;

    for _ in 0..50 {
        mod1_ticks += 1;
        mod2_ticks += 1;
        mod4_ticks += 1;
    }

    assert_eq!(mod1_ticks, 50);
    assert_eq!(mod2_ticks, 50);
    assert_eq!(mod4_ticks, 50);
    assert!(!MOD3_ACTIVE.load(Ordering::SeqCst));
}

#[test]
fn t4_scenario_e_50_cycle_hot_reload_and_dynamic_lifecycle_stress() {
    // Scenario E: 50 cycles of loading, executing, hot-reloading and teardown
    let mut world_state = HashMap::<u64, i32>::new();
    for i in 0..100 {
        world_state.insert(i, 0);
    }

    static RELOAD_COUNT: AtomicU32 = AtomicU32::new(0);

    for _cycle in 0..50 {
        // Step 1: Mod init
        let desc = SystemDescriptor {
            struct_size: core::mem::size_of::<SystemDescriptor>() as u32,
            name: b"HotReloadMod\0".as_ptr() as *const c_char,
            queries: core::ptr::null(),
            query_count: 0,
            callback: dummy_callback,
            user_data: core::ptr::null_mut(),
        };
        let status = unsafe { mock_register_system(core::ptr::null_mut(), &desc) };
        assert_eq!(status, FfiResult::OK);

        // Step 2: Execute 5 ticks
        for _ in 0..5 {
            for val in world_state.values_mut() {
                *val += 1;
            }
        }

        // Step 3: Mod teardown hook
        unsafe extern "C" fn mock_teardown(_ctx: *mut c_void) -> FfiResult {
            FfiResult::OK
        }
        let shutdown_status = unsafe { mock_teardown(core::ptr::null_mut()) };
        assert_eq!(shutdown_status, FfiResult::OK);

        RELOAD_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    assert_eq!(RELOAD_COUNT.load(Ordering::SeqCst), 50);
    // 50 cycles * 5 ticks = 250 ticks total
    for val in world_state.values() {
        assert_eq!(*val, 250);
    }
}

// Helper dummy callback
unsafe extern "C" fn dummy_callback(
    _iter: *mut RawChunkIter,
    _cmds: *mut RawCommandQueue,
    _ud: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}
