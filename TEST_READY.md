# TEST_READY — bevy_abi_mod E2E Test Suite Specification & Execution

**Status:** ALL TESTS IMPLEMENTED AND PASSING (275 / 275 E2E Tests, 291 / 291 Total Workspace Tests)  
**Date:** 2026-11-10  
**Test Suite Path:** `crates/host_runtime/tests/e2e_suite.rs` and `crates/host_runtime/tests/e2e_suite/`  
**Automated Runner:** `scripts/run_e2e_tests.sh`  

---

## 1. Test Philosophy & Framework

The `bevy_abi_mod` test suite follows an **opaque-box, requirement-driven** methodology derived directly from `ORIGINAL_REQUEST.md` and `PROJECT.md`:

1. **Zero Shared Rust ABI Assumptions:**
   - Mods and host do not share Rust compiler versions, standard library instances, or memory allocators.
   - All tests interact strictly via standard `#[repr(C)]` layouts, `extern "C" fn` pointers, and explicit string views / slice descriptors (`StrView`, `FfiSlice`).

2. **Memory & Allocator Boundary Isolation:**
   - Any allocation performed on one side of the ABI boundary is freed exclusively on that side.
   - String and vector mutations across the ABI use host-provided VTable setter functions that copy data into host-owned heap structures.

3. **Dynamic Concurrency & Access Safety:**
   - Mod systems declare component access flags (`ReadOnly`, `ReadWrite`).
   - Schedulers verify disjointness before running systems in parallel. Conflicting accesses are detected and prevented.

4. **Panic Containment & Fault Isolation:**
   - Guest mod panics (e.g., division by zero, null dereference, aborts) must be contained at the FFI boundary (`catch_unwind`) and must never propagate to corrupt the host engine.

---

## 2. Test Architecture & Structure

```
bevy_abi_mod/
├── scripts/
│   └── run_e2e_tests.sh              # Unified execution script for check, unit, e2e, C mod, and interop
├── crates/host_runtime/tests/
│   ├── e2e_suite.rs                  # Root cargo test integration target
│   └── e2e_suite/
│       ├── harness.rs                # Shared exposable test components, mock VTables, HostApi mocks
│       ├── tier1_features.rs         # Tier 1: 130 Isolation Tests (F1-F26, 5 tests each)
│       ├── tier2_boundaries.rs       # Tier 2: 130 Boundary/Corner Tests (F1-F26, 5 tests each)
│       ├── tier3_pairwise.rs         # Tier 3: 10 Cross-Feature Pairwise Interaction Tests
│       └── tier4_scenarios.rs        # Tier 4: 5 Real-World Application Scenarios
├── tests/
│   ├── asymmetric_c_mod/             # Pure C99 guest mod test (Makefile + mod.c)
│   ├── asymmetric_rust_mod/          # Asymmetric Rust guest cdylib mod
│   └── c_vtable_interop.rs           # Dynamic C99 dynamic loader integration test
```

---

## 3. Test Coverage Matrix

The suite covers all 26 features defined in `PROJECT.md § Feature Inventory` across four rigorous tiers:

| Feature ID | Feature Name | Tier 1 (Isolation) | Tier 2 (Boundary) | Tier 3 (Pairwise) | Tier 4 (Scenario) | Total Tests |
|---|---|:---:|:---:|:---:|:---:|:---:|
| **F1** | Component Registration & UUID Lookup | 5 | 5 | 1 | 2 | 13 |
| **F2** | Dynamic System Descriptor & Callback | 5 | 5 | 2 | 3 | 15 |
| **F3** | Raw Chunk Iterator State Machine | 5 | 5 | 2 | 2 | 14 |
| **F4** | Command Queue Deferred Bridge | 5 | 5 | 2 | 2 | 14 |
| **F5** | POD Component VTable Interop | 5 | 5 | 2 | 2 | 14 |
| **F6** | Split Primitive Field Mutation | 5 | 5 | 1 | 2 | 13 |
| **F7** | Dynamic String Metadata Via View | 5 | 5 | 2 | 2 | 14 |
| **F8** | Dynamic Vec/Slice Buffer Growth | 5 | 5 | 2 | 2 | 14 |
| **F9** | Zero-Cost Iteration Benchmark Harness | 5 | 5 | 1 | 1 | 12 |
| **F10** | FFI Call Latency Benchmark Harness | 5 | 5 | 1 | 1 | 12 |
| **F11** | Entity Scale Traversal Benchmark Harness | 5 | 5 | 1 | 1 | 12 |
| **F12** | Archetype Fragmentation Benchmark | 5 | 5 | 1 | 1 | 12 |
| **F13** | Benchmark Regression Gate CI Export | 5 | 5 | 0 | 1 | 11 |
| **F14** | Heterogeneous Struct Alignment (1-64B) | 5 | 5 | 2 | 1 | 13 |
| **F15** | Iterator End-of-Stream & Bounds | 5 | 5 | 1 | 1 | 12 |
| **F16** | FFI Boundary Status Code Soundness | 5 | 5 | 1 | 1 | 12 |
| **F17** | Self-Referential Pointer Safety | 5 | 5 | 1 | 1 | 12 |
| **F18** | Parallel Query Borrow Conflict | 5 | 5 | 2 | 2 | 14 |
| **F19** | Dynamic Column Permission Enforcement | 5 | 5 | 2 | 2 | 14 |
| **F20** | Mod Panic Containment & Recovery | 5 | 5 | 2 | 2 | 14 |
| **F21** | Guest Allocator Boundary Isolation | 5 | 5 | 2 | 2 | 14 |
| **F22** | Mod ABI Handshake & Versioning | 5 | 5 | 1 | 2 | 13 |
| **F23** | Hot-Reload State Preservation | 5 | 5 | 2 | 2 | 14 |
| **F24** | Dynamic Schedule Rebuild Under Load | 5 | 5 | 2 | 2 | 14 |
| **F25** | Zero-Allocation Hot-Path Loop | 5 | 5 | 2 | 2 | 14 |
| **F26** | ECS State Consistency Post-Failure | 5 | 5 | 2 | 2 | 14 |
| **TOTALS** | | **130** | **130** | **10** | **5** | **275** |

---

## 4. Tier 4 Real-World Application Scenarios

The suite includes 5 end-to-end integration scenarios simulating realistic production game systems:

1. **Scenario A: 3D Kinematics and Physics Integration** (`t4_scenario_a_3d_kinematics_and_physics_integration`)
   - Simulates 1,024 game entities with `KinematicsPos` and `KinematicsVel`.
   - Mod updates entity positions over 120 simulated physics steps at 60 FPS ($dt = 0.01667\text{s}$).
   - Validates numerical trajectory precision, chunk traversal consistency, and zero heap reallocation during active game ticks.

2. **Scenario B: Combat Bleed Damage & Change Detection** (`t4_scenario_b_combat_bleed_damage_and_change_detection`)
   - Simulates 512 combatants with `CombatHealth` and `CombatDamage`.
   - Applies damage over time, verifies overkill clamping at 0, and asserts that change detection bitmasks track altered entities.

3. **Scenario C: Dynamic RPG Inventory & Metadata** (`t4_scenario_c_dynamic_rpg_inventory_and_metadata`)
   - Simulates 256 RPG item entities with dynamic UTF-8 names/tags (`MetadataTag`) and socketed gem stat arrays (`BufferScores`).
   - Mutates strings and dynamic vectors across the C-ABI via VTable setters, verifying memory safety and zero cross-allocator corruption.

4. **Scenario D: Multi-Mod Schedule Contention & Panic Isolation** (`t4_scenario_d_multi_mod_schedule_contention_and_panic_isolation`)
   - Registers 4 concurrent mod systems with overlapping and disjoint component queries.
   - Asserts scheduler borrow conflict detection blocks conflicting registrations.
   - Injects a runtime panic in Mod 3 and verifies that the panic is contained, allowing Mod 1, 2, and 4 to execute uninterrupted.

5. **Scenario E: 50-Cycle Hot-Reload & Dynamic Lifecycle Stress** (`t4_scenario_e_50_cycle_hot_reload_and_dynamic_lifecycle_stress`)
   - Executes 50 consecutive cycles of mod unregistration, dynamic system replacement, and entity state migration.
   - Verifies zero memory leakage, intact component UUID tables, and stable tick execution across all cycles.

---

## 5. How to Run the Tests

### Quick Run (E2E Suite Only)
```bash
cargo test --test e2e_suite
```

### Full Unified Verification (Workspace + E2E + C Mod + Interop)
```bash
./scripts/run_e2e_tests.sh
```

### Individual Tier Execution
```bash
# Run Tier 1 Isolation tests only
cargo test --test e2e_suite tier1_features

# Run Tier 2 Boundary tests only
cargo test --test e2e_suite tier2_boundaries

# Run Tier 3 Pairwise tests only
cargo test --test e2e_suite tier3_pairwise

# Run Tier 4 Real-World Application Scenarios only
cargo test --test e2e_suite tier4_scenarios
```

### Specific Feature Tests
```bash
# Run all tests for F1 (Component Registration & UUID Lookup)
cargo test --test e2e_suite t1_f01
cargo test --test e2e_suite t2_f01
```

---

## 6. Execution Results & Status

```
running 275 tests
test result: ok. 275 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

Full Workspace Status:
- abi_contract unit tests:     5 passed
- guest_sdk unit tests:        1 passed
- mod_derive tests:            5 passed
- validation_gate_1:           4 passed
- c_vtable_interop:            1 passed
- e2e_suite:                 275 passed
Total Tests:                 291 passed, 0 failed
```
