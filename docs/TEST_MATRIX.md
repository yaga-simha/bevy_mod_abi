# Host Integration Test Matrix (`bevy_abi_mod_host`)

This document tracks the behavior-organized host integration test architecture (`T-01`, `N-03`), hermetic `.cdylib` fixture workflow (`T-02`), throughput benchmark separation (`T-04`), and complete behavior/regression traceability from the historical 31-binary suite.

---

## 1. Consolidated Integration Test Binaries (`T-01` & `N-03`)

`crates/bevy_abi_mod_host/Cargo.toml` sets `autotests = false` and defines **4 multi-module integration test binaries** organized strictly by runtime subsystem behavior (plus shared support in `tests/support/helpers.rs`):

| Binary Target | Entry Point | Submodules | Covered Behaviors |
|---|---|---|---|
| `ffi` | `tests/ffi/main.rs` | `regressions`, `boundary_validation`, `engine_bridge`, `sparse_and_resources`, `strides_and_alignment`, `chunk_and_commands`, `c_vtable_interop`, `uuid_parity` | `#[repr(C)]` descriptor validation & struct-size negotiation, `ModRegistry` & `host_get_proc_address`, SoA chunk iteration, `SparseSet` columns, `Optional`/`Changed`/`Added` filters, ECS resources, deferred `CommandQueueBridge` operations, heterogeneous alignment/padding/ZST strides, C99 VTable mutation interop, and 128-bit FNV-1a `ComponentUuid` parity. |
| `scheduling` | `tests/scheduling/main.rs` | `regressions`, `conflicts_and_faults`, `multi_query_and_conditions`, `differential_parity` | Parallel disjoint execution, write/write & read/write borrow conflict detection (guest-guest & guest-native), `MultiQuery` access validation & resource scoping, `run_if` conditions, named `ModNamedSystemSet` ordering & isolation, `Startup` vs runtime schedule dispatch, panic/error fault isolation & `ModErrorPolicy`, and side-by-side differential parity against native Bevy ECS. |
| `lifecycle` | `tests/lifecycle/main.rs` | `regressions`, `handshake_and_strict_load`, `drop_trampolines`, `hot_reload_and_excision` | SemVer ABI handshake (`ABI_VERSION_PACKED`), `LoadTransaction` atomic rollback on init/strict validation failure, post-unload stale VTable invalidation, foreign drop trampoline slot allocation/reclamation/panic containment, 50–1000 cycle hot-reload excision without schedule graph bloat, and multi-app concurrent load/unload churn. |
| `e2e` | `tests/e2e/main.rs` | `harness`, `tier1_features`, `tier2_boundaries`, `tier3_pairwise`, `tier4_scenarios`, `asymmetric_mods`, `example_pipelines` | End-to-end 4-tier feature/boundary/pairwise/scenario verification matrix (`t1_*`–`t4_*`), simultaneous asymmetric Rust + C99 `.cdylib` mods, cross-mod runtime dynamic components, and Example 01–04 visual/kinematics/pipeline behavior oracles. |

---

## 2. Hermetic `.cdylib` Fixture Workflow (`T-02`)

Integration tests **never** invoke `cargo`, `make`, `cc`, `gcc`, or `clang` (`std::process::Command::new` is completely absent from `crates/*/tests/**`). All guest shared libraries are built ahead-of-time into `target/fixtures/` via `cargo xtask`:

```bash
# Build core Rust + C99 integration test fixtures into target/fixtures/
cargo xtask fixtures

# Also stage Example 01–04 guest mod .cdylibs into target/fixtures/
cargo xtask fixtures --all

# Run the entire workspace test suite hermetically
cargo test --workspace -- --test-threads=1
```

### Fixture Resolution (`BAM_FIXTURE_DIR`)
`tests/support/helpers.rs::fixture_dylib(stem)` resolves `<prefix><stem>.<ext>` from `BAM_FIXTURE_DIR` (defaulting to `<workspace_root>/target/fixtures` when unset). If a required `.cdylib` is absent, tests fail immediately with:

```text
Fixture missing at <path> — run 'cargo xtask fixtures' before 'cargo test'
```

### Prebuilt Fixtures Staged in `target/fixtures/`

| Fixture Stem | Source Path | Used By |
|---|---|---|
| `asymmetric_rust_mod` | `tests/asymmetric_rust_mod/` | `e2e`, `lifecycle`, `bevy_abi_mod_guest` tests, `chunk_throughput` bench |
| `asymmetric_c_mod` | `tests/asymmetric_c_mod/` | `ffi::c_vtable_interop`, `e2e`, `lifecycle` |
| `c_uuid_oracle` | `tests/fixtures/c_mods/c_uuid_oracle.c` | `ffi::uuid_parity` |
| `s05_vtable_mod` | `tests/fixtures/c_mods/s05_vtable_mod.c` | `lifecycle::regressions` (`S-05`) |
| `s06_fail_init` | `tests/fixtures/c_mods/s06_fail_init.c` | `lifecycle::regressions` (`S-06`) |
| `s06_strict_fail` | `tests/fixtures/c_mods/s06_strict_fail.c` | `lifecycle::regressions` (`S-06`) |
| `l01_late_startup` | `tests/fixtures/c_mods/l01_late_startup.c` | `scheduling::regressions` (`L-01`) |
| `mod_alpha`, `mod_beta` | `tests/fixtures/c_mods/mod_{alpha,beta}.c` | `scheduling::regressions` (`L-05a`) |
| `mod_producer`, `mod_consumer` | `tests/fixtures/c_mods/mod_{producer,consumer}.c` | `scheduling::regressions` (`L-05b`) |
| `l05c_clean_mod` | `tests/fixtures/c_mods/l05c_clean_mod.c` | `scheduling::regressions` (`L-05c`) |
| `minimal_guest_mod` | `examples/01_minimal_handshake/guest_mod/` | `e2e::example_pipelines` (`--examples` / `--all`) |
| `example_02_guest_mod` | `examples/02_multi_system_pipeline/guest_mod/` | `e2e::example_pipelines` (`--examples` / `--all`) |
| `example_03_rust_mod`, `example_03_c_mod` | `examples/03_cross_language_interop/{rust_mod,c_mod}/` | `e2e::example_pipelines` (`--examples` / `--all`) |
| `mod_gravity`, `mod_rainbow`, `mod_vortex` | `examples/04_visual_game/mods/*/` | `e2e::example_pipelines` (`--examples` / `--all`) |

---

## 3. Wall-Clock Timing Separation (`T-04`)

All wall-clock `Instant::now()` / `elapsed()` timing assertions were removed from `crates/*/tests/**`. Throughput and micro-overhead measurements from the legacy `tests/benchmarks.rs` harness now live exclusively in `crates/bevy_abi_mod_host/benches/chunk_throughput.rs` (`cargo bench -p bevy_abi_mod_host --bench chunk_throughput`).

---

## 4. Wave 1 Regression Suite Mapping (All 14 `regression_*` Tests Preserved Verbatim)

Every `regression_*` function from `wave1_regressions.rs` is preserved with its exact function name:

| Regression Function | Issue ID | New Binary | New Module Path |
|---|---|---|---|
| `regression_s04_padded_struct_insert` | `S-04` | `ffi` | `tests/ffi/regressions.rs` |
| `regression_s10_entity_bits_layout` | `S-10` | `ffi` | `tests/ffi/regressions.rs` |
| `regression_s05_stale_vtable_after_unload` | `S-05` | `lifecycle` | `tests/lifecycle/regressions.rs` |
| `regression_s06_init_failure_rolls_back` | `S-06` | `lifecycle` | `tests/lifecycle/regressions.rs` |
| `regression_s07_register_outside_init_rejected` | `S-07` | `lifecycle` | `tests/lifecycle/regressions.rs` |
| `regression_l01_runtime_loaded_startup_runs_once` | `L-01` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_l05a_cross_mod_name_isolation` | `L-05a` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_l05b_later_mod_after_earlier_system` | `L-05b` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_l05c_named_sets_removed_on_unload` | `L-05c` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_b04_too_many_terms` | `B-04` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_b04_invalid_mode` | `B-04` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_b04_unknown_component` | `B-04` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_b04_kind_mismatch` | `B-04` | `scheduling` | `tests/scheduling/regressions.rs` |
| `regression_b04_self_conflict` | `B-04` | `scheduling` | `tests/scheduling/regressions.rs` |

---

## 5. Legacy 31-Binary to 4-Binary Traceability Matrix

| Legacy Test File | New Destination(s) | Naming / Consolidation Notes |
|---|---|---|
| `adversarial_challenge.rs` | `scheduling::conflicts_and_faults`, `ffi::chunk_and_commands` | Fault isolation & compatible read/disjoint tests -> `scheduling`; multi-frame/multi-table change detection -> `ffi`; duplicate basic 2-system write/write & write/read panic tests deduplicated (see §6). |
| `adversarial_m3_m4_challenge.rs` | `ffi::chunk_and_commands` | Both 100k/multi-archetype arithmetic oracle tests preserved in `multi_archetype_scaling_oracle`. |
| `benchmarks.rs` | `benches/chunk_throughput.rs` | Wall-clock timing harness moved out of `tests/` per `T-04`. |
| `bevy_alignment_soundness_tests.rs` | `ffi::sparse_and_resources`, `ffi::boundary_validation`, `scheduling::conflicts_and_faults`, `scheduling::multi_query_and_conditions`, `scheduling::differential_parity`, `lifecycle::handshake_and_strict_load`, `lifecycle::drop_trampolines`, `lifecycle::hot_reload_and_excision` | All 16 soundness/alignment/world-isolation/lifecycle tests preserved across their respective domain modules. |
| `bevy_phase3_phase4_features.rs` | `ffi::sparse_and_resources`, `scheduling::conflicts_and_faults`, `scheduling::multi_query_and_conditions`, `e2e::asymmetric_mods` | All 11 tests preserved; `test_end_to_end_asymmetric_rust_and_c99_dylibs_phase3_phase4` renamed to `test_end_to_end_asymmetric_rust_and_c99_optional_changed_resources_and_commands`. |
| `c_vtable_interop.rs` | `ffi::c_vtable_interop` | Replaced `Command::new("make")` with `fixture_dylib("asymmetric_c_mod")`. |
| `challenger_m1_c_boundary_tests.rs` | `ffi::boundary_validation` | All 6 `HostApi` struct-size negotiation & concurrent `get_proc_address` tests preserved. |
| `challenger_m1_dynamic_descriptor_stress.rs` | `ffi::boundary_validation` | 6 unique layout/UTF-8/offset/vtable/extreme-limit tests preserved; 5 subsets subsumed by `fuzz_boundaries` (see §6). |
| `challenger_m1_plugin_and_commands.rs` | `ffi::engine_bridge` | All 3 `AbiModPlugin` & typed command bridge tests preserved. |
| `challenger_m2_concurrency_stress.rs` | `scheduling::conflicts_and_faults`, `lifecycle::drop_trampolines`, `ffi::strides_and_alignment` | All 9 tests preserved; `test_challenge_*` prefix normalized to behavior names (`test_*`). |
| `challenger_m2_empirical_stress.rs` | `ffi::strides_and_alignment`, `lifecycle::drop_trampolines` | 8 unique tests preserved (`test_adversarial_*` normalized to `test_*`); 1 duplicate slot-reuse test consolidated into `test_drop_trampoline_slot_reuse_cross_talk` (see §6). |
| `challenger_m3_2_empirical_stress.rs` | `lifecycle::drop_trampolines`, `lifecycle::hot_reload_and_excision`, `ffi::chunk_and_commands` | 9 unique tests preserved; 1 redundant 15-cycle reload loop subsumed by 50-cycle hot-reload test (see §6). |
| `challenger_m3_adversarial.rs` | `e2e::example_pipelines` | All 5 Example 01–03 pipeline tests preserved (`test_adversarial_example_*` renamed to `test_minimal_handshake_*`, `test_multi_system_pipeline_*`, `test_cross_language_c99_and_rust_*`) using prebuilt fixtures. |
| `challenger_m3_concurrent_schedule_stress.rs` | `scheduling::multi_query_and_conditions` | `test_concurrent_dual_mod_schedule_stress` preserved. |
| `challenger_m3_uuid_parity.rs` | `ffi::uuid_parity` | Replaced runtime C compiler invocation with prebuilt `c_uuid_oracle` fixture. |
| `challenger_m4_adversarial.rs` | `e2e::example_pipelines` | Subsumed 1:1 by the stricter 10-test Example 04 oracle suite in `e2e::example_pipelines` (see §6). |
| `challenger_m5_1_stress_tests.rs` | `ffi::chunk_and_commands`, `scheduling::multi_query_and_conditions`, `e2e::asymmetric_mods` | 8 unique tests preserved; Example 03 duplicate consolidated into `e2e::example_pipelines` (see §6). |
| `challenger_m5_visual_game_stress.rs` | `e2e::example_pipelines` | All 10 Example 04 visual game physics/color/vortex/shadow-copy/churn tests preserved (`test_adversarial_*` -> `test_visual_game_*`). |
| `challenger_p2_stress_tests.rs` | `ffi::chunk_and_commands` | All 9 chunk traversal, bitmask change detection, and 10k burst command tests preserved. |
| `differential_bevy_parity.rs` | `scheduling::differential_parity` | All 4 native-vs-guest differential parity tests preserved. |
| `e2e_suite.rs` (`tests/e2e_suite/*`) | `e2e::{harness,tier1_features,tier2_boundaries,tier3_pairwise,tier4_scenarios}` | All 275 `t1_*` (130), `t2_*` (130), `t3_*` (10), and `t4_*` (5) tests preserved with wall-clock timing assertions removed (`T-04`). |
| `fuzz_abi_boundaries.rs` | `ffi::boundary_validation` | All 5 exhaustive boundary fuzz matrices preserved. |
| `integration_tests.rs` | `e2e::asymmetric_mods` | All 10 asymmetric C99 + Rust integration tests preserved (`test_validation_gate_3_*` & `test_milestone_*` renamed to behavior names). |
| `lifecycle_tests.rs` | `lifecycle::hot_reload_and_excision` | All 3 hot-reload, post-unload, and guest-failure consistency tests preserved. |
| `m1_adversarial_stress_tests.rs` | `ffi::chunk_and_commands` | All 9 archetype table scaling, permission enforcement, and boundary tests preserved. |
| `m1_engine_bridge_tests.rs` | `ffi::engine_bridge` | All 11 `ModRegistry`, `host_get_proc_address`, `CommandQueueBridge`, and `DynamicCSystem` tests preserved. |
| `m2_dynamic_ecs_tests.rs` | `ffi::strides_and_alignment`, `lifecycle::drop_trampolines` | 5 unique dynamic ECS tests preserved (`test_m2_*` -> `test_*`); 1 duplicate conflict test subsumed (see §6). |
| `phase3_stress_challenge.rs` | `lifecycle::handshake_and_strict_load`, `lifecycle::hot_reload_and_excision`, `scheduling::conflicts_and_faults` | 7 unique lifecycle/concurrency/panic tests preserved; 2 duplicates consolidated (see §6). |
| `safety_tests.rs` | `ffi::strides_and_alignment`, `ffi::boundary_validation`, `scheduling::conflicts_and_faults`, `scheduling::multi_query_and_conditions` | 6 unique alignment/transparent-FFI/allocator/panic/multi-query tests preserved; 1 duplicate conflict test subsumed (see §6). |
| `validation_gate_2.rs` | `ffi::engine_bridge` | Preserved and renamed to `test_host_runtime_10k_entities_soa_and_deferred_commands`. |
| `wave1_regressions.rs` | `ffi::regressions`, `scheduling::regressions`, `lifecycle::regressions` | All 14 `regression_*` tests preserved verbatim with prebuilt C99 fixtures (see §4). |

---

## 6. Explicit Deduplication Ledger

Only genuinely redundant tests whose assertions are 100% covered by a stricter retained test in the same binary/suite were deduplicated:

| Removed Legacy Test | Legacy File | Retained Covering Test(s) | Coverage Justification |
|---|---|---|---|
| `test_borrow_conflict_write_write_detection`, `test_borrow_conflict_write_read_detection`, `test_borrow_conflict_detection_across_guest_systems`, `test_m2_concurrency_permissions_and_conflict_detection` | `adversarial_challenge.rs`, `safety_tests.rs`, `m2_dynamic_ecs_tests.rs` | `scheduling::conflicts_and_faults::test_write_write_and_write_read_conflict_detection`, `scheduling::differential_parity::differential_query_access_compatibility_matrix_parity` | Retained tests verify write-write, write-read, read-read, `FilterOnly`, and full pairwise access compatibility matrices against Bevy's scheduler. |
| `test_zst_descriptors_boundary`, `test_invalid_alignments_fuzzing`, `test_misaligned_sizes_matrix`, `test_struct_size_negotiation_boundary`, `test_null_pointers_boundary` | `challenger_m1_dynamic_descriptor_stress.rs` | `ffi::boundary_validation::fuzz_boundaries::fuzz_host_register_component_layouts_and_contexts`, `ffi::boundary_validation::host_api_boundary_and_proc_address::test_host_register_component_extensive_stress_matrix` | Both retained matrices test ZSTs, non-power-of-two/zero alignments, size%align!=0, truncated `struct_size`, and null pointers across every registration entrypoint. |
| `test_adversarial_drop_trampoline_unload_and_slot_reuse_behavior` | `challenger_m2_empirical_stress.rs` | `lifecycle::drop_trampolines::{test_non_pod_slot_reuse_on_mod_reload, test_drop_trampoline_slot_reuse_cross_talk}` | Retained tests cover unload tombstoning, slot reclamation on reload, and multi-component cross-talk isolation. |
| `test_multilanguage_mod_reload_churn_cycles` (15 cycles), `test_stress_rapid_mod_reload_and_unload_cycling` (25 cycles) | `challenger_m3_2_empirical_stress.rs`, `phase3_stress_challenge.rs` | `lifecycle::hot_reload_and_excision::{test_50_cycle_hot_reload_lifecycle_stress, hot_reload_1000_cycles_remove_systems_in_set_keeps_schedule_graph_constant}` | Retained tests run 50 full dual-language `.cdylib` load/tick/unload cycles and 1,000 schedule excision cycles. |
| `test_stress_high_scale_multi_archetype` | `phase3_stress_challenge.rs` | `ffi::chunk_and_commands::multi_archetype_scaling_oracle::test_100k_scale_fragmented_archetype_traversal_and_arithmetic`, `e2e::asymmetric_mods::test_high_throughput_archetype_permutations` | Retained tests validate fragmented multi-archetype traversal at 100k entities and with live dual-language `.cdylib` mods. |
| `test_stress_example_3_concurrency_and_lifecycle` | `challenger_m5_1_stress_tests.rs` | `e2e::example_pipelines::test_cross_language_c99_and_rust_concurrency_and_asymmetric_unload` | Both load `rust_kinematics_mod` + `c_combat_mod`, tick in parallel, unload asymmetrically, and verify surviving entity health/position invariants. |
| `test_m4_gravity_mod_physics_and_bounce`, `test_m4_rainbow_mod_color_spectrum_and_mutation`, `test_m4_vortex_mod_orbital_swirl_and_repulsion`, `test_m4_rapid_switching_across_all_mods_and_reloads`, `test_m4_stress_1000_entities_across_all_mods`, `test_m4_empty_world_chunk_iteration_safety`, `test_m4_shadow_copy_isolation_and_drop_cleanup` | `challenger_m4_adversarial.rs` | `e2e::example_pipelines::{test_visual_game_gravity_physical_kinematics_oracle, test_visual_game_rainbow_color_spectrum_oracle, test_visual_game_vortex_orbital_cyclone_oracle, test_visual_game_rapid_mod_cycling_50_cycles, test_visual_game_scale_2500_entities, test_visual_game_boundary_empty_world, test_visual_game_shadow_copying_and_file_lock_bypass}` | Every `m4` Example 04 test has a stricter 1-to-1 counterpart (exact mathematical kinematics/HSL/orbital oracle, 50 cycles vs 15, 2,500 entities vs 1,000) in `e2e::example_pipelines`. |
