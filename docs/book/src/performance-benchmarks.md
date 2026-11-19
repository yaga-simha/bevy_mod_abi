# Performance Tuning & Benchmarks

`bevy_abi_mod` is engineered so decoupled native mods run at native Bevy speed within 60 Hz (`16.67 ms`) and 120 Hz (`8.33 ms`) frame budgets. Full hardware baselines, ABI `0.3.0` migration/hot-rewire/soak gates, and commands live in [`docs/BENCHMARKS.md`](../../BENCHMARKS.md).

---

## 1. Six Golden Rules for Mod Performance

1. **Iterate by Chunk & Use `BAM_RESTRICT`, Never Chatty Per-Entity FFI Calls:**
   Crossing `extern "C"` once per Bevy archetype table (`Chunk::component_mut`, `Chunk::read_write`, or `iter_column_mutable` with `BAM_RESTRICT` in C/C++/Zig) amortizes FFI dispatch over hundreds or thousands of contiguous entities and unlocks LLVM SIMD auto-vectorization (`0.15–0.33 ns/entity` at `100k` entities on Apple M4 Pro).
2. **Pre-Resolve `BamComponent` Handles Once in `init`:**
   Resolve every component, relation, and resource handle during `GuestPlugin::build` / `bevy_abi_mod_init`. Per-frame commands (`cmd_insert`, `cmd_spawn`) index `RegistrySnapshot` in $O(1)$ with zero UTF-8 checks or string hashing.
3. **Prefer `#[repr(C)]` `AbiPod` for High-Frequency Numeric Data:**
   Use `#[repr(C)]` `f32`/`u32` structs with `SCHEMA_FINGERPRINT` for transforms, velocities, physics state, and spatial sensors so guests operate on contiguous slices directly, reload with **0 bytes copied**, and expose declarative `BamFieldDesc` reflection at zero hot-path cost. Reserve VTable getters/setters for cold metadata (`String` labels, variable-length `Vec<T>` inventories).
4. **Use Batched `cmd_spawn` / `cmd_spawn_batch` Instead of `spawn_empty` + `N` `insert`s:**
   `GuestCommands::spawn(&entries)` (`host_cmd_spawn`) copies all `K` component payloads into the system's reusable 64 KiB `CommandArena` (`MaybeUninit<u8>`, `0` steady-state heap allocations) and inserts them via `EntityWorldMut::insert_by_ids` in **one** archetype transition (`~1.66x` faster than sequential spawn + 4 inserts; `cmd_spawn_batch` is `~2.6x` faster still).
5. **Leverage Untracked Mutable Slices & `BAM_COMPONENT_FLAG_SUMMARY_TICK`:**
   Calling `iter_column_mutable` marks the chunk's change ticks once per chunk. If a system inspects `100,000` entities and mutates only `< 1%` of them, use `column_mut_untracked` + `mark_changed_range(term, row, 1)` paired with `.summary_tick()` so downstream `Changed<T>` systems skip whole unchanged tables in $O(1)$.
6. **Use `par_for_each_chunk_readonly` & `.chain_weak()` for Maximum Multi-Core Throughput:**
   Read-only parallel passes (`par_chunks_readonly`) allocate zero per-worker `HostCommands`/`CommandArena` slabs, and `BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK` (`.chain_weak()`) avoids injecting redundant scheduler barriers between non-conflicting sibling systems.

---

## 2. Canonical Benchmarks, Soak & Regression Gates

Rather than duplicating numbers across documents, the complete **9-scenario benchmark suite** (`chunk_throughput.rs`), **Apple M4 Pro hardware reference baselines**, **deterministic `iai-callgrind` + zero-allocation CI gates (`benches/deterministic_gate`)**, **34-archetype permutation matrix (`benches/permutation_bench`)**, and **ABI `0.3.0` migration / hot-rewire / schedule-shuffle / 10,000-reload soak gates** are maintained in one place in [`docs/BENCHMARKS.md`](../../BENCHMARKS.md).

---

## 3. Running Benchmarks & Regression Gates Locally

```bash
# Run all 9 canonical scenarios (builds Rust & C99 fixtures first)
cargo xtask bench

# Run quick mode locally
cargo bench -p bevy_abi_mod_host --bench chunk_throughput -- --quick

# Run deterministic zero-allocation & step-count / iai-callgrind gate
cargo bench --manifest-path benches/deterministic_gate/Cargo.toml

# Run ABI 0.3.0 schedule-shuffle fuzz & 10,000-reload soak gates
cargo test -p bevy_abi_mod_host --test schedule_shuffle_fuzz --all-features -- --nocapture
cargo test --release -p bevy_abi_mod_host --test soak_10k_reloads --all-features -- --nocapture
```
