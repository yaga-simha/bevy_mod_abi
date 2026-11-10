# Testing & Verification Framework: `bevy_abi_mod`

This document details the multi-tiered testing methodology, safety audits, and automated verification pipelines of **`bevy_abi_mod`**.

---

## 1. Test Architecture Overview

`bevy_abi_mod` employs a rigorous, layered testing methodology ensuring ABI soundness, concurrency safety, zero allocator leakage, and high throughput.

```
┌────────────────────────────────────────────────────────────────────────┐
│                   Layer 4: Verification Pipeline                       │
│      scripts/run_e2e_tests.sh (9 Automated Sequential Stages)          │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
    ┌───────────────────────────────┼───────────────────────────────┐
    ▼                               ▼                               ▼
┌────────────────────────┐  ┌────────────────────────┐  ┌────────────────────────┐
│  Multi-Language Mods   │  │  Safety & Lifecycle    │  │  Performance Bench     │
│  - C99 shared library  │  │  - 50-cycle hot-reload │  │  - 1k, 10k, 100k scale │
│  - Rust cdylib mod     │  │  - Borrow conflict det │  │  - Archetype splitting │
│  - Dual concurrent run │  │  - Memory alignment    │  │  - JSON reports export │
└────────────────────────┘  └────────────────────────┘  └────────────────────────┘
                                    │
    ┌───────────────────────────────┴───────────────────────────────┐
    ▼                                                               ▼
┌──────────────────────────────────────────┐  ┌──────────────────────────────────┐
│  Tiered E2E Suite (275 Tests)            │  │  Validation Gates 1, 2, 3        │
│  - Tier 1: Core Features                 │  │  - Gate 1: Proc-macro VTables    │
│  - Tier 2: Boundary Safety               │  │  - Gate 2: 10k Host Runtime      │
│  - Tier 3: Pairwise Matrix Combinations  │  │  - Gate 3: Live Bevy Integration │
│  - Tier 4: Real-world Game Scenarios     │  └──────────────────────────────────┘
└──────────────────────────────────────────┘
```

---

## 2. The 9-Stage Verification Pipeline

The complete verification pipeline is executed with `make test-e2e` or `bash scripts/run_e2e_tests.sh`:

| Stage | Target / Test | Objective | Success Metric |
|---|---|---|---|
| **[1/9]** | `make -C tests/asymmetric_c_mod`<br>`cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release` | Compile independent guest shared libraries (`.so`/`.dylib`/`.dll` and `.cdylib`). | Zero compilation errors. |
| **[2/9]** | `cargo check --workspace --all-targets` | Static typing and API boundary compliance across all crates. | 0 warnings, 0 errors. |
| **[3/9]** | `cargo test --workspace -- --test-threads=1` | Unit tests for `abi_contract`, `mod_derive`, `host_runtime`, and `guest_sdk`. | All unit tests pass. |
| **[4/9]** | `cargo test --test e2e_suite` | 275 tests across 4 comprehensive tiers. | 275/275 passed. |
| **[5/9]** | `cargo test --test validation_gate_2` | 10,000 entities chunk iteration, `Changed<Position>`, deferred command bridge. | Gate 2 passed. |
| **[6/9]** | `cargo test --test integration_tests` | Validation Gate 3: Multi-archetype Bevy App with live C99 and Rust mods. | Gate 3 passed. |
| **[7/9]** | `cargo test --test benchmarks -- --nocapture` | High-scale throughput benchmarks (1k, 10k, 100k entities) with JSON report. | `target/benchmark_results.json` generated. |
| **[8/9]** | `cargo test --test safety_tests` | Alignment (1B..64B), pointer bounds, borrow conflicts, memory isolation. | All safety checks pass. |
| **[9/9]** | `cargo test --test lifecycle_tests` | 50 continuous hot-reload cycles and post-unload immunity. | 0 memory corruption, clean teardown. |

---

## 3. Four-Tier E2E Test Suite (`tests/e2e_suite.rs`)

The E2E suite contains **275 discrete tests** organized into four structured tiers:

### Tier 1: Isolated Feature Tests (35 Tests)
- **T1_F01**: Component UUID lookup and VTable symbol resolution.
- **T1_F02**: Dynamic system descriptors and callback invocations.
- **T1_F03**: Table chunk traversal, entity slices, column strides.
- **T1_F04**: Command queue bridge (`spawn_empty`, `despawn`, `insert_bytes`).
- **T1_F05**: Lifecycle management and library unmapping.
- **T1_F06**: Change detection tick propagation.
- **T1_F07**: Foreign panic containment and status conversion.

### Tier 2: Boundary Safety & Stress (35 Tests)
- Null pointer handling across all host FFI functions.
- Empty worlds, empty tables, and single-entity tables.
- Out-of-bounds column indices and invalid access modes.
- Mutating read-only columns rejected with `PERMISSION_DENIED`.
- Rapid command bursts (10,000 commands/sec).

### Tier 3: Pairwise Combinatorial Matrix (105 Tests)
- All combinations of:
  - System counts: 1, 2, 4, 8 systems.
  - Access modes: READ, WRITE, FILTER_ONLY, READ+WRITE.
  - Archetype distribution: 1, 4, 16 archetypes.
  - Entity scales: 0, 10, 100, 1,000, 10,000 entities.

### Tier 4: Real-World Game Scenarios (100 Tests)
- 3D Kinematics and physics integration.
- Combat damage, status effect decay, and death despawns.
- Inventory item attachments and metadata inspection.
- Simultaneous execution of asymmetric C99 and Rust mods modifying common entities.

---

## 4. Safety & Adversarial Suites

### 4.1 Borrow Conflict Detection (`safety_tests.rs`)
Bevy's ECS requires that two systems mutating the same component cannot run in parallel.
The safety suite validates that:
- Registering two systems with `WRITE` access to `Position` triggers Bevy's conflict detection.
- Systems with disjoint access (`SystemA: WRITE Position`, `SystemB: WRITE Velocity`) execute in parallel without conflict.

### 4.2 Heterogeneous Memory Alignment (`safety_tests.rs`)
Validates that pointer arithmetic respects alignment for structures ranging from 1-byte tags (`u8`) to 64-byte SIMD vectors (`align_of = 64`), preventing misaligned CPU faults.

### 4.3 50-Cycle Hot-Reloading Stress (`lifecycle_tests.rs`)
Loads and unloads guest shared libraries in a continuous 50-cycle loop:
- Validates that `dlclose` unmaps library text and data segments cleanly.
- Asserts that host ECS tables and native systems continue uninterrupted after mod unloading without segmentation faults or memory corruption.

---

## 5. Performance Benchmarks

Performance is quantitatively verified across varying entity counts and archetype distributions.

To run benchmarks and inspect results:
```bash
make bench
cat target/benchmark_results.json
```

### Benchmark Metrics Baseline & Full Permutation Matrix
- **Single-Query Linear Scale (`benchmarks.rs`)**:
  - **100,000 Entities Throughput**: ~**20.3M+ entities/sec** in debug host, >**250M+ entities/sec** in release host.
  - **Archetype Fragmentation**: Traversal remains consistent across 1-way, 4-way, and 16-way fragmented archetypes.
- **Full Multi-Architecture × `Debug`/`Release` Nested Query Matrix (`benches/permutation_bench`, [`docs/BENCHMARKS.md`](BENCHMARKS.md))**:
  - Evaluates a **34-archetype** world (16 active unit tables × 6 active emitter tables + 12 excluded decoy/frozen tables; nested `Transform3D` & `Kinematics` `Vec3f` sub-structs) at `49,152` and `294,912` pair evaluations/frame.
  - Verifies mathematical oracle equivalence across Pure Native Bevy, Coupled Rust `dylib`, C-ABI Rust `cdylib`, and C-ABI C99 across all `Debug` and `Release` targets, mapping the full `1.00x` (`0.186 ms`) -> `66.47x` (`12.39 ms`) performance degradation ladder.
