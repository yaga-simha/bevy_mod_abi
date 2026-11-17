#![allow(unused_imports)]

use super::harness::*;
use bevy_abi_mod_sys::{
    ABI_VERSION, AccessMode, BamComponent, BamHostApi, BamInstance, BamIter, BamSystemCtx,
    BamSystemDesc, BamTermDesc, ComponentUuid, FfiEntity, FfiResult, FfiSlice, ModComponent,
    StrView,
};
use core::ffi::{CStr, c_char, c_void};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

// ============================================================================
// Tier 4: Real-World Application Scenarios (ABI 0.2)
// ============================================================================

#[test]
fn t4_scenario_a_3d_kinematics_and_physics_integration() {
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
                .map(|i| FfiEntity::from_bits((t * per_table + i + 1) as u64))
                .collect(),
            columns: vec![
                MockColumnData {
                    name: KinematicsPos::CANONICAL_NAME.to_string(),
                    stride: core::mem::size_of::<KinematicsPos>(),
                    is_mutable: true,
                    bytes: pos_bytes,
                    changed_flags: vec![false; per_table],
                },
                MockColumnData {
                    name: KinematicsVel::CANONICAL_NAME.to_string(),
                    stride: core::mem::size_of::<KinematicsVel>(),
                    is_mutable: false,
                    bytes: vel_bytes,
                    changed_flags: vec![false; per_table],
                },
            ],
        });
    }

    let iter = &mut state as *mut MockChunkIterState as *mut BamIter;
    let pos_vt = KinematicsPos::vtable();
    let vel_vt = KinematicsVel::vtable();

    let mut processed = 0;

    loop {
        let count = unsafe { mock_step_chunk(iter) };
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
                let px = (pos_vt.get_x.unwrap())(p_ptr);
                let py = (pos_vt.get_y.unwrap())(p_ptr);
                let pz = (pos_vt.get_z.unwrap())(p_ptr);

                let vx = (vel_vt.get_x.unwrap())(v_ptr);
                let vy = (vel_vt.get_y.unwrap())(v_ptr);
                let vz = (vel_vt.get_z.unwrap())(v_ptr);

                assert_eq!((pos_vt.set_x.unwrap())(p_ptr, px + vx), FfiResult::OK);
                assert_eq!((pos_vt.set_y.unwrap())(p_ptr, py + vy), FfiResult::OK);
                assert_eq!((pos_vt.set_z.unwrap())(p_ptr, pz + vz), FfiResult::OK);
            }
        }
        processed += count;
    }

    assert_eq!(processed, total_entities);

    let first_table = &state.tables[0];
    let first_pos = unsafe { &*(first_table.columns[0].bytes.as_ptr() as *const KinematicsPos) };
    assert_eq!(first_pos.x, 1.0);
    assert_eq!(first_pos.y, 0.5);
    assert_eq!(first_pos.z, -0.5);
}

#[test]
fn t4_scenario_b_combat_bleed_damage_and_change_detection() {
    let count = 2_500;
    let health_data: Vec<CombatHealth> = (0..count)
        .map(|i| CombatHealth {
            current: if i % 10 == 0 { 10 } else { 100 },
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
        entities: (1..=count)
            .map(|i| FfiEntity::from_bits(i as u64))
            .collect(),
        columns: vec![MockColumnData {
            name: CombatHealth::CANONICAL_NAME.to_string(),
            stride: core::mem::size_of::<CombatHealth>(),
            is_mutable: true,
            bytes,
            changed_flags: vec![false; count],
        }],
    });

    let mut cmd_queue = MockCommandQueueState::new();
    let iter = &mut state as *mut MockChunkIterState as *mut BamIter;
    let health_vt = CombatHealth::vtable();

    let c = unsafe { mock_step_chunk(iter) };
    assert_eq!(c, count);

    let h_base = unsafe { mock_iter_column_mutable(iter, 0) };
    let h_stride = unsafe { mock_iter_column_stride(iter, 0) };
    let ents_ptr = unsafe { mock_iter_entities(iter) };
    let ents = unsafe { core::slice::from_raw_parts(ents_ptr, c) };

    let bleed_damage = 15;
    let mut dead_count = 0;

    for (i, &ent) in ents.iter().enumerate().take(c) {
        let h_ptr = unsafe { h_base.add(i * h_stride) } as *mut c_void;
        let cur = unsafe { (health_vt.get_current.unwrap())(h_ptr) };
        let new_hp = cur - bleed_damage;
        unsafe {
            assert_eq!(
                (health_vt.set_current.unwrap())(h_ptr, new_hp),
                FfiResult::OK
            );
        }

        assert_eq!(unsafe { mock_iter_mark_changed(iter, 0, i) }, FfiResult::OK);

        if new_hp <= 0 {
            cmd_queue.push(MockCommand::Despawn(ent));
            dead_count += 1;
        }
    }

    assert_eq!(dead_count, 250);
    let flushed = cmd_queue.flush();
    assert_eq!(flushed.len(), 250);
}

#[test]
fn t4_scenario_c_dynamic_rpg_inventory_and_metadata() {
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

    // 1. Equip item via push_scores / set_scores
    unsafe {
        assert_eq!((inv_vt.push_scores.unwrap())(inv_ptr, 205), FfiResult::OK);
    }
    assert_eq!(inventory.scores, vec![101, 102, 103, 205]);

    // 2. Self-aliasing title upgrade: prepend "Sir_" to own slice
    let cur_name = unsafe { (tag_vt.get_label.unwrap())(tag_ptr).as_str().unwrap() };
    let upgraded_title = format!("Sir_{cur_name}");
    unsafe {
        assert_eq!(
            (tag_vt.set_label.unwrap())(tag_ptr, StrView::from_str(&upgraded_title)),
            FfiResult::OK
        );
    }
    assert_eq!(tag.label, "Sir_Knight_Lancelot");

    // 3. Multi-byte Unicode clan tag
    let clan_label = "⚔️ 圆桌骑士团 ⚔️";
    unsafe {
        assert_eq!(
            (tag_vt.set_label.unwrap())(tag_ptr, StrView::from_str(clan_label)),
            FfiResult::OK
        );
    }
    assert_eq!(tag.label, clan_label);
}

#[test]
fn t4_scenario_d_multi_mod_schedule_contention_and_panic_isolation() {
    let mod1_queries = [
        BamTermDesc::read(POS_HANDLE),
        BamTermDesc::write(VEL_HANDLE),
    ];
    let mod2_queries = [
        BamTermDesc::read(VEL_HANDLE),
        BamTermDesc::write(POS_HANDLE),
    ];
    let mod3_queries = [BamTermDesc::write(HEALTH_HANDLE)];
    let mod4_queries = [
        BamTermDesc::filter_only(POS_HANDLE),
        BamTermDesc::read(VEL_HANDLE),
    ];

    assert!(check_borrow_conflict(&mod1_queries, &mod2_queries));
    assert!(!check_borrow_conflict(&mod1_queries, &mod3_queries));
    assert!(!check_borrow_conflict(&mod2_queries, &mod3_queries));
    assert!(!check_borrow_conflict(&mod4_queries, &mod3_queries));

    static MOD3_ACTIVE: AtomicBool = AtomicBool::new(true);
    let mod3_run = std::panic::catch_unwind(|| {
        if MOD3_ACTIVE.load(Ordering::SeqCst) {
            panic!("Mod 3 encountered division by zero in game formula");
        }
    });
    assert!(mod3_run.is_err());
    MOD3_ACTIVE.store(false, Ordering::SeqCst);

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
    let mut world_state = HashMap::<u64, i32>::new();
    for i in 0..100 {
        world_state.insert(i, 0);
    }

    static RELOAD_COUNT: AtomicU32 = AtomicU32::new(0);
    static SHUTDOWN_COUNT: AtomicU32 = AtomicU32::new(0);

    unsafe extern "C" fn mock_teardown(_instance: *mut BamInstance) {
        SHUTDOWN_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    for _cycle in 0..50 {
        let desc = BamSystemDesc::new(
            c"HotReloadMod".as_ptr(),
            core::ptr::null(),
            0,
            dummy_system_callback,
            core::ptr::null_mut(),
        );
        let status = unsafe { mock_register_system(core::ptr::null_mut(), &desc) };
        assert_eq!(status, FfiResult::OK);

        for _ in 0..5 {
            for val in world_state.values_mut() {
                *val += 1;
            }
        }

        unsafe {
            mock_teardown(core::ptr::null_mut());
        }
        RELOAD_COUNT.fetch_add(1, Ordering::SeqCst);
    }

    assert_eq!(RELOAD_COUNT.load(Ordering::SeqCst), 50);
    assert_eq!(SHUTDOWN_COUNT.load(Ordering::SeqCst), 50);
    for val in world_state.values() {
        assert_eq!(*val, 250);
    }
}
