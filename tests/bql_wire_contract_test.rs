#![forbid(unsafe_code)]

//! End-to-end differential wire contract tests verifying bit-identical execution between strict
//! ISO C99 (`include/bevy_abi_mod.h`) and `#![forbid(unsafe_code)]` Safe Rust (`bevy_abi_mod_guest`)
//! guests across the complete `bevy_bql` ABI `0.3.0` runtime contract.

use std::sync::OnceLock;

use bevy_app::FixedUpdate;
use bevy_mod_abi::prelude::*;
use example_09_bql_wire_parity::{
    DEFAULT_MIGRATE_FRAME, DEFAULT_TOTAL_FRAMES, EXPECTED_ORDER_TRACE, FrameAccumulator,
    ParityDsos, SimClock, TOTAL_HIERARCHY_NODES, assert_reflect_parity,
    assert_wire_schema_metadata_parity, build_parity_app, build_parity_dsos, hash_world_state,
    run_differential_parity,
};

fn shared_parity_dsos() -> &'static ParityDsos {
    static DSOS: OnceLock<ParityDsos> = OnceLock::new();
    DSOS.get_or_init(build_parity_dsos)
}

#[test]
fn bql_c99_vs_safe_rust_500_frame_differential_wire_parity() {
    let dsos = shared_parity_dsos();
    let report = run_differential_parity(dsos, DEFAULT_TOTAL_FRAMES, DEFAULT_MIGRATE_FRAME);

    assert_eq!(report.verified_frames, 500);
    assert_eq!(report.migrated_rows, TOTAL_HIERARCHY_NODES);
    assert_eq!(report.symbol_count, 7);
    assert_eq!(report.final_accumulator.order_trace, EXPECTED_ORDER_TRACE);
    assert_eq!(
        report.final_accumulator.readonly_count,
        TOTAL_HIERARCHY_NODES as u64
    );
    assert_eq!(report.final_accumulator.committed_tx_count, 99);
    assert_eq!(report.final_accumulator.rolled_back_tx_count, 499);

    // Live v1 -> v2 column migration expanded 596 `NodeState` rows by +4 bytes each (+2384 bytes).
    assert_eq!(
        report.post_migration_digest.pod_bytes_hashed - report.pre_reload_digest.pod_bytes_hashed,
        TOTAL_HIERARCHY_NODES * 4,
        "v1 (12B) -> v2 (16B) migration must expand NodeState column by exactly 4 bytes per live node"
    );

    println!(
        "[PASS] 500/500 frames bit-identical | migrated={} rows (12B->16B) | \
         frame0={} | frame249={} | migrated={} | frame499={}",
        report.migrated_rows,
        report.frame_0_digest.sha256_hex(),
        report.pre_reload_digest.sha256_hex(),
        report.post_migration_digest.sha256_hex(),
        report.final_digest.sha256_hex(),
    );
}

#[test]
fn bql_atomic_rollback_and_cascade_delete_leave_zero_leaked_entities() {
    let dsos = shared_parity_dsos();
    let mut c99_app = build_parity_app();
    let mut rust_app = build_parity_app();

    let load_opts = LoadOptions {
        shadow_copy: true,
        strict: true,
        ..Default::default()
    };
    let c_id = c99_app
        .load_mod_with(&dsos.c99_v1, load_opts.clone())
        .expect("load C99 v1");
    let r_id = rust_app
        .load_mod_with(&dsos.rust_v1, load_opts)
        .expect("load Rust v1");

    assert_wire_schema_metadata_parity(c99_app.world(), rust_app.world(), false);

    // Run 10 frames (0..10): covers bootstrap (0), rollback-only (1,3,6,8), commit+EXCLUSIVE (5),
    // explicit `cmd_remove_relation` + extra satellite (7), and `CASCADE_DELETE` despawn (9).
    for frame in 0..10u32 {
        c99_app.world_mut().resource_mut::<SimClock>().frame = frame;
        rust_app.world_mut().resource_mut::<SimClock>().frame = frame;
        c99_app.world_mut().run_schedule(FixedUpdate);
        rust_app.world_mut().run_schedule(FixedUpdate);

        let c_digest = hash_world_state(c99_app.world());
        let r_digest = hash_world_state(rust_app.world());
        assert_eq!(c_digest, r_digest, "divergence on frame {frame}");

        let c_tx_comp = c99_app
            .world()
            .resource::<ModRegistry>()
            .get_by_name("bql_parity::TxToken")
            .expect("TxToken")
            .component_id;
        let tx_count: usize = c99_app
            .world()
            .archetypes()
            .iter()
            .filter(|a| a.contains(c_tx_comp))
            .map(|a| a.len() as usize)
            .sum();

        let rel_participants = c_digest.relation_participant_count;

        match frame {
            0..=4 => {
                // Only `perm_sat` exists (every rolled-back tx leaked 0 entities and 0 edges).
                assert_eq!(
                    tx_count, 1,
                    "frame {frame}: rolled-back tx must leak 0 TxTokens"
                );
                assert_eq!(
                    rel_participants, 2,
                    "frame {frame}: only perm_sat + root target expected"
                );
            }
            5 | 6 => {
                // `perm_sat` + `active_anchor` + `active_sat` = 3 TxTokens, 4 participating entities
                // (`EXCLUSIVE` evicted `roots[1]` so only `perm_sat`, `roots[0]`, `active_sat`, `active_anchor` remain).
                assert_eq!(tx_count, 3, "frame {frame}: expected 3 TxTokens");
                assert_eq!(
                    rel_participants, 4,
                    "frame {frame}: EXCLUSIVE must evict roots[1], leaving exactly 4 participating entities"
                );
            }
            7 | 8 => {
                // `extra_sat` added after `cmd_remove_relation` + re-attach = 4 TxTokens, 5 participants.
                assert_eq!(tx_count, 4, "frame {frame}: expected 4 TxTokens");
                assert_eq!(
                    rel_participants, 5,
                    "frame {frame}: expected 5 participating entities"
                );
            }
            9 => {
                // `cmd_despawn(active_anchor)` cascaded to BOTH `active_sat` and `extra_sat`!
                assert_eq!(
                    tx_count, 1,
                    "frame 9: CASCADE_DELETE must despawn anchor + both dependent satellites"
                );
                assert_eq!(
                    rel_participants, 2,
                    "frame 9: only persistent perm_sat + root target must remain after CASCADE_DELETE"
                );
            }
            _ => unreachable!(),
        }
    }

    assert_reflect_parity(c99_app.world_mut(), rust_app.world_mut(), false);
    assert_eq!(
        c99_app
            .world()
            .resource::<FrameAccumulator>()
            .rolled_back_tx_count,
        9
    );
    assert_eq!(
        c99_app
            .world()
            .resource::<FrameAccumulator>()
            .committed_tx_count,
        1
    );

    c99_app.world_mut().unload_mod(c_id).expect("unload C99");
    rust_app.world_mut().unload_mod(r_id).expect("unload Rust");
}
