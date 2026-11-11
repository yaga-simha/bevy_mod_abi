# Testing & Verification Framework: `bevy_abi_mod`

This document details the behavioral test organization, differential Bevy parity suites, adversarial C-ABI boundary fuzzing, compile-fail harnesses, sanitizer/Miri gates, and automated CI pipelines of **`bevy_abi_mod`**.

---

## 1. Test Architecture & Organization Map

`bevy_abi_mod` organizes tests by **architectural invariant and behavior** rather than historical milestones, ensuring ABI soundness, Bevy ECS scheduler/storage parity, zero allocator crossover, and high throughput.

```text
┌──────────────────────────────────────────────────────────────────────────────────┐
│                       Continuous Integration (.github/workflows/ci.yml)          │
│  • OS Matrix (Linux/macOS/Windows)   • Strict ISO C99 (-Wall -Wextra -Werror)    │
│  • Miri (-Zmiri-tree-borrows)        • ASan / UBSan C99   • -Zrandomize-layout   │
└────────────────────────────────────────┬─────────────────────────────────────────┘
                                         │
       ┌─────────────────────────────────┼─────────────────────────────────┐
       ▼                                 ▼                                 ▼
┌────────────────────────────┐  ┌────────────────────────────┐  ┌────────────────────────────┐
│ Behavioral & Parity Suites │  │ Boundary Fuzz & Compile    │  │ Live Asymmetric Mods &     │
│ - differential_bevy_parity │  │ - fuzz_abi_boundaries      │  │ Lifecycle / E2E Suites     │
│ - bevy_alignment_soundness │  │ - compile_fail_tests       │  │ - asymmetric_c_mod (.so)   │
│ - validation_gate_1 / _2   │  │ - standalone_c_test.c      │  │ - asymmetric_rust_mod      │
└────────────────────────────┘  └────────────────────────────┘  └────────────────────────────┘
```

### Complete Test Suite Directory & Responsibility Matrix

| Crate / Location | Test Target | Primary Behavioral Responsibility | Miri-Compatible |
|---|---|---|---|
| `crates/abi_contract` | `--lib` & doc-tests | `#[repr(C)]` struct sizes/offsets, `ExtensibleStruct` / `read_descriptor` truncation & zeroed-fn checks, `FfiBool`, `FfiEntity::NULL`, SemVer `abi_version_is_compatible`. | Yes |
| `crates/mod_derive` | `validation_gate_1.rs`<br>`challenger_m1_macro_tests.rs` | Generated `#[repr(C)]` VTables, `FfiBool` (`u8`) accessors, self-aliasing `String`/`Vec<T>` overlap safety, `IS_POD` / `IS_RAW_INSERTABLE` derivation. | Yes |
| `crates/mod_derive` | **`compile_fail_tests.rs`** | Real `rustc` subprocess compile-fail suite verifying `#[derive(AbiPod)]` rejects missing `#[repr(C)]`, `packed`, `bool`, `String`, `Vec<u8>`, enums, generics, and `Drop`, and `ModExposable` with `String`/`bool` fails `: AbiPod` / `: FfiLayout` bounds. | Guarded (`#[cfg_attr(miri, ignore)]`) |
| `crates/guest_sdk` | `--lib` & doc-tests | `GuestApp` transactional rollback & LIFO `cleanup_registered_guest_state`, `Chunk` split-borrow non-overlap (`read_write`, `columns_mut`, `write_2`), `catch_guest_panic`. | Yes |
| `crates/host_runtime` | **`differential_bevy_parity.rs`** | Deterministic PRNG differential oracle comparing native Bevy `Query` + `Commands` vs. `DynamicCSystem` across `Table` + `SparseSet` schemas, ZSTs, over-aligned types, multi-frame `Changed<T>`, `World::check_change_ticks()` `u32::MAX` wraparound, `ChildOf` recursive despawn, `EntityCloner` (`ComponentCloneBehavior::Ignore`), and multi-`World` `std::thread::scope` executor + drop-trampoline isolation. | Yes (scaled iterations) |
| `crates/host_runtime` | **`fuzz_abi_boundaries.rs`** | Deterministic SplitMix64 adversarial fuzzer hammering all 16 `host_*` `extern "C"` entry points across thousands of malformed inputs: `struct_size` sweeps `0..128` (aligned & misaligned), null pointers, non-UTF8 C strings, non-power-of-two alignments, `usize::MAX` overflow sizes, invalid `ComponentAccessMode`/`ScheduleLabelId`, `query_count > 32`, `query_spec_count > 8`, forged/null/placeholder `FfiEntity`, and raw-insert permission denials. | Yes (scaled iterations) |
| `crates/host_runtime` | `bevy_alignment_soundness_tests.rs` | Targeted soundness regressions (`D1`–`D10`): shared-table `SparseSet` filters/columns, `world.increment_change_tick()`, `ModSystemSet` 1,000-cycle `remove_systems_in_set`, `ScheduledSystemDescriptor` ordering, `ModErrorPolicy`, partial multi-mod unload drop safety. | Yes (except 1 `dlopen` test) |
| `crates/host_runtime` | `integration_tests.rs`, `lifecycle_tests.rs`, `safety_tests.rs`, `e2e_suite.rs` | Live `.dylib`/`.so`/`.dll` loading of `tests/asymmetric_c_mod` and `tests/asymmetric_rust_mod`, 50-cycle hot-reload stress, 275-case 4-tier E2E matrix. | Host native (`dlopen`) |
| `tests/` | `standalone_c_test.c` | Pure ISO C99 compile-time `BEVY_ABI_STATIC_ASSERT` layout checks, legacy 64-byte vs. 72-byte `HostApi` forward/backward compatibility, `BAM_ENTITY_NULL`, SemVer helpers, and `ScheduledSystemDescriptor`. | Native C99 + ASan/UBSan |

---

## 2. Deep Dive: Differential, Fuzz & Compile-Fail Suites

### 2.1 Differential Native-vs-Dynamic Bevy Parity (`differential_bevy_parity.rs`)
Rather than asserting hand-picked point values alone, `differential_bevy_parity.rs` treats **Bevy ECS itself as the ground-truth specification**:
1. **Access-Compatibility Matrix Parity**: Generates random single-query and multi-query schemas mixing `StorageType::Table`, `StorageType::SparseSet`, ZSTs (`size == 0`), and over-aligned (`align(16)`, `align(32)`) components across `READ`, `WRITE`, `FILTER_ONLY`, and `WITHOUT`. Verifies 100% agreement between `DynamicCSystem::initialize` and native Bevy `QueryBuilder` on both intra-system self-conflicts and pairwise `FilteredAccessSet::is_compatible` / `ScheduleBuildSettings { ambiguity_detection: LogLevel::Error }` matrices.
2. **Side-by-Side World Execution & Tick Wraparound**: Seeds identical `World A` (native Bevy `Query` + `Commands`) and `World B` (`DynamicCSystem` single + multi-query + `host_iter_reset` + `host_cmd_*`), asserting identical entity sets, component values, and `Changed<T>` observations across multi-frame schedules, `CHECK_TICK_THRESHOLD` (`World::check_change_ticks()`) clamping scans, and `u32::MAX -> 0` tick wraparound boundaries.
3. **Hierarchy & `EntityCloner` Drop Safety**: Verifies 5-node `ChildOf` tree recursive despawn parity (`Commands::despawn` vs. `host_cmd_despawn`) with exact foreign `ComponentDropFn` counts, plus `clone_and_spawn()` cloning static/dynamic POD components while ignoring foreign-droppable dynamic components (`ComponentCloneBehavior::Ignore`, zero double-free).
4. **Multi-World Parallel `std::thread::scope` Isolation**: Runs concurrent `World`s on Bevy's multi-threaded executor, verifying worker-thread `host_get_proc_address` symbol isolation (`CURRENT_REGISTRY`) and independent per-`(WorldId, ComponentUuid)` drop-trampoline counts.

### 2.2 Adversarial C-ABI Boundary Fuzzing (`fuzz_abi_boundaries.rs`)
Deterministically fuzzes all 16 `host_*` `extern "C"` functions (`host_register_system`, `host_register_scheduled_system`, `host_register_system_in_schedule`, `host_register_multi_query_system`, `host_register_component`, `host_iter_*`, `host_cmd_*`, `host_get_proc_address`):
- Exhaustively sweeps `struct_size` across `0..128` and `u32::MAX` inside exact-length heap allocations (both aligned and 1-byte misaligned) so reading even 1 byte past `struct_size` or assuming pointer alignment is caught immediately by Miri/ASan.
- Injects zeroed function-pointer slots (`Option<extern "C" fn>` niche check before `assume_init`), invalid UTF-8 C strings, non-power-of-two alignments, `size % align != 0`, `usize::MAX` overflow sizes, `query_count > 32`, `query_spec_count > 8`, invalid `ComponentAccessMode(4..=u32::MAX)` and `ScheduleLabelId(6..=u32::MAX)`, forged/null/placeholder `FfiEntity` bits, and raw-byte insertions onto `String`/`bool`/`opaque` components—asserting zero panics, zero UB, and clean `FfiResult` error codes.

### 2.3 Real `rustc` Compile-Fail Suite (`compile_fail_tests.rs`)
Invokes `rustc --edition=2021 --crate-type=lib --emit=metadata` against the compiled `abi_contract` and `mod_derive` artifacts to verify both procedural macro diagnostics and Rust type-system rejections:
- `#[derive(AbiPod)]` rejects structs missing `#[repr(C)]`, `#[repr(C, packed)]`, `bool` fields, `String` fields, `Vec<u8>` fields, enums, generic structs, direct `Drop` impls, and transitive `Drop` fields.
- `#[derive(ModExposable)]` structs containing `String`, `bool`, `#[repr(Rust)]` layout, or `#[mod_component(no_pod)]` do **not** implement `AbiPod` (and cannot be nested inside `Vec<BadElem>` in another `ModExposable` component).

---

## 3. Developer Verification Commands & CI Gates

### 3.1 Local `Makefile` Verification Targets
```bash
# Run full workspace unit, parity, fuzz, compile-fail, integration & doc tests
make test

# Compile C99 mod & run standalone_c_test.c with -std=c99 -Wall -Wextra -Werror
make test-c99

# Run strict Clippy linter (-D warnings) across all workspace targets
make clippy

# Run Miri (-Zmiri-tree-borrows) across abi_contract, mod_derive, guest_sdk & host_runtime
make miri

# Execute the 9-stage E2E pipeline (builds mods, runs unit/E2E/gate/safety/lifecycle suites)
make test-e2e
```

### 3.2 Continuous Integration Workflow (`.github/workflows/ci.yml`)
Every push and pull request runs five automated jobs:
1. **`test-matrix` (`ubuntu-latest`, `macos-latest`, `windows-latest`)**: Builds both asymmetric C99 and Rust guest mods, checks `rustfmt` and `clippy -D warnings`, and runs `cargo test --workspace -- --test-threads=1`.
2. **`c99-strict` (`gcc` & `clang`)**: Compiles all C99 guest/benchmark/example mods and executes `standalone_c_test.c` under `-std=c99 -Wall -Wextra -Werror`, plus verifies C++17 header compatibility (`-std=c++17 -Wall -Wextra -Werror`).
3. **`miri`**: Executes `cargo +nightly miri test` with `MIRIFLAGS="-Zmiri-tree-borrows"` across `abi_contract`, `guest_sdk`, `mod_derive`, and `host_runtime` non-`dlopen` soundness, fuzz, and differential parity suites.
4. **`sanitizers`**: Runs `standalone_c_test.c` under AddressSanitizer + UndefinedBehaviorSanitizer (`-fsanitize=address,undefined -fno-sanitize-recover=all`) and executes host C-mod integration tests against a UBSan-instrumented `libasymmetric_c_mod.so`.
5. **`randomize-layout`**: Compiles the guest `.cdylib` with default layout and runs the host workspace under `RUSTFLAGS="-Zrandomize-layout"`, empirically proving zero reliance on unstable `#[repr(Rust)]` struct field ordering across the ABI.

---

## 4. The 9-Stage E2E Verification Pipeline (`scripts/run_e2e_tests.sh`)

Executed with `make test-e2e` or `bash scripts/run_e2e_tests.sh`:

| Stage | Target / Test | Objective | Success Metric |
|---|---|---|---|
| **[1/9]** | `make -C tests/asymmetric_c_mod`<br>`cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release` | Compile independent guest shared libraries (`.so`/`.dylib`/`.dll` and `.cdylib`). | Zero compilation errors. |
| **[2/9]** | `cargo check --workspace --all-targets` | Static typing and API boundary compliance across all crates. | 0 warnings, 0 errors. |
| **[3/9]** | `cargo test --workspace -- --test-threads=1` | Unit, parity, fuzz, compile-fail, and doc tests across all crates. | All tests pass. |
| **[4/9]** | `cargo test --test e2e_suite` | 275 tests across 4 comprehensive tiers (Core, Boundary, Pairwise Matrix, Game Scenarios). | 275/275 passed. |
| **[5/9]** | `cargo test --test validation_gate_2` | 10,000 entities chunk iteration, `Changed<Position>`, deferred command bridge. | Gate 2 passed. |
| **[6/9]** | `cargo test --test integration_tests` | Multi-archetype Bevy App with live C99 and Rust mods. | Gate 3 passed. |
| **[7/9]** | `cargo test --test benchmarks -- --nocapture` | High-scale throughput benchmarks (1k, 10k, 100k entities) with JSON report. | `target/benchmark_results.json` generated. |
| **[8/9]** | `cargo test --test safety_tests` | Alignment (`1B..64B`), pointer bounds, borrow conflicts, memory isolation. | All safety checks pass. |
| **[9/9]** | `cargo test --test lifecycle_tests` | 50 continuous hot-reload cycles and post-unload immunity. | 0 memory corruption, clean teardown. |

---

## 5. Performance Benchmarks & Methodology

```bash
# Run full cross-architecture & debug/release permutation benchmark matrix:
make bench

# Run Milestone 4 single-query linear throughput test (1k, 10k, 100k entities):
make bench-quick
```

- **Single-Query Linear Scale (`benchmarks.rs`)**: ~**20.3M+ entities/sec** in debug host, >**100M+ entities/sec** in release host across 1-way, 4-way, and 16-way fragmented archetypes.
- **Full Multi-Architecture × `Debug`/`Release` Nested Query Matrix ([`docs/BENCHMARKS.md`](BENCHMARKS.md))**: Demonstrates **`1.00x`–`1.15x` native parity** for Chunk-Optimized C99 (`0.186 ms`) and Rust `cdylib` (`0.215 ms`) vs. Pure Native Bevy (`0.186 ms` SoA / `0.218 ms` nested) without compiler lock-in.
