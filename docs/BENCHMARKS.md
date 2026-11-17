# Benchmarks & Deterministic Regression Gates (`ABI 0.2.0`)

This document records the **canonical benchmark scenarios**, **deterministic CI regression gates (`iai-callgrind` + counting allocator)**, **reference hardware baselines**, and the **full 5-dimensional permutation matrix** for `bevy_abi_mod` (`ABI 0.2.0`).

```bash
# 1. Run all 5 canonical host benchmark scenarios (stages Rust & C99 fixtures automatically)
cargo xtask bench

# 2. Quick local run (short warmup / 12 rounds) or CI smoke check (--test)
cargo bench -p bevy_abi_mod_host --bench chunk_throughput -- --quick
cargo bench -p bevy_abi_mod_host -- --test

# 3. Deterministic regression gate (iai-callgrind on Linux CI; 0-alloc + step-count gate on macOS/Windows)
cargo bench --manifest-path benches/deterministic_gate/Cargo.toml

# 4. Full 34-archetype Debug x Release x Native/dylib/C-ABI permutation ladder
cargo xtask bench --permutation
```

---

## 1. Reference Machine Specifications

All baseline measurements below were recorded on the same local Apple Silicon workstation:

| Property | Specification |
|---|---|
| **SoC / CPU** | Apple M4 Pro (14 cores: performance + efficiency clusters, `aarch64-apple-darwin`) |
| **System Memory** | 48 GiB Unified Memory (`51,539,607,552` bytes) |
| **Operating System** | macOS (`arm64-apple-darwin27.2.0`) |
| **Rust Toolchain** | `rustc 1.98.1` (`stable-aarch64-apple-darwin`), `--release` / `bench` (`opt-level = 3`) |
| **C / C++ Toolchain** | Apple Clang `21.0.0` (`clang-2100.4.1.1`), `-std=c99 -O3` |
| **Bevy ECS Version** | `bevy_ecs 0.20.1` (`bevy_app 0.20.1`) |

---

## 2. Canonical Benchmark Suite (`crates/bevy_abi_mod_host/benches/chunk_throughput.rs`)

`chunk_throughput.rs` unifies the five core `ABI 0.2.0` runtime paths into a single reproducible suite.

### 2.1 Scenario 1: Dense SoA Chunk Iteration (`1k` / `10k` / `100k` Entities)
Compares native Bevy `Query<(&mut Position, &Velocity)>`, direct C-ABI `GuestSystem` contiguous SoA slice iteration (`host_iter_column_mutable` + `host_iter_column_readonly`), and loaded `.cdylib`/`.dylib` dynamic components (`RustModCustomData` and `CModCustomData`):

| Scale | Implementation | Median Frame | p95 Frame | Throughput (`ent/s`) | `ns / entity` |
|---:|---|---:|---:|---:|---:|
| **`1,000`** | Native Bevy (`Pos + Vel`) | `0.0020 ms` | `0.0021 ms` | `505M ent/s` | `1.96 ns` |
| **`1,000`** | C-ABI `GuestSystem` (SoA Slice) | **`0.0014 ms`** | `0.0015 ms` | **`715M ent/s`** | **`1.38 ns`** |
| **`1,000`** | Asymmetric Rust `.cdylib` (Dynamic) | `0.0016 ms` | `0.0017 ms` | `621M ent/s` | `1.62 ns` |
| **`1,000`** | Asymmetric ISO C99 `.dylib` (Dynamic) | `0.0015 ms` | `0.0016 ms` | `649M ent/s` | `1.54 ns` |
| **`10,000`** | Native Bevy (`Pos + Vel`) | `0.0100 ms` | `0.0100 ms` | `999M ent/s` | `1.00 ns` |
| **`10,000`** | C-ABI `GuestSystem` (SoA Slice) | **`0.0040 ms`** | `0.0041 ms` | **`2.48B ent/s`** | **`0.40 ns`** |
| **`10,000`** | Asymmetric Rust `.cdylib` (Dynamic) | **`0.0028 ms`** | `0.0030 ms` | **`3.54B ent/s`** | **`0.28 ns`** |
| **`10,000`** | Asymmetric ISO C99 `.dylib` (Dynamic) | `0.0047 ms` | `0.0047 ms` | `2.14B ent/s` | `0.47 ns` |
| **`100,000`** | Native Bevy (`Pos + Vel`) | `0.0900 ms` | `0.1111 ms` | `1.06B ent/s` | `0.90 ns` |
| **`100,000`** | C-ABI `GuestSystem` (SoA Slice) | **`0.0263 ms`** | `0.0265 ms` | **`3.80B ent/s`** | **`0.26 ns`** |
| **`100,000`** | Asymmetric Rust `.cdylib` (Dynamic) | **`0.0148 ms`** | `0.0149 ms` | **`6.75B ent/s`** | **`0.15 ns`** |
| **`100,000`** | Asymmetric ISO C99 `.dylib` (Dynamic) | `0.0334 ms` | `0.0370 ms` | `2.97B ent/s` | `0.33 ns` |

> **Why SoA Chunk Slices Beat Per-Row Native `Query` Iteration:** Native `for (mut p, v) in &mut query` constructs a `Mut<Position>` wrapper and updates individual row change ticks inside the scalar loop, inhibiting NEON auto-vectorization. `host_iter_column_mutable` marks the table chunk's change ticks once and hands LLVM a contiguous `&mut [Position]` slice that vectorizes into 128-bit NEON SIMD loads/stores.

### 2.2 Scenario 2: `Changed<Position>` Tick Filter (`100,000` Entities @ `1%`, `50%`, `100%`)
Validates summary-tick optimization (once-per-table `ComponentTicks` resolution, summary-tick whole-table skipping, and branch-light contiguous run coalescing):

| Mutation Rate (`100k` Entities) | Implementation | Median Frame | p95 Frame | Effective Scan Rate | `ns / entity` |
|---|---|---:|---:|---:|---:|
| **`1%` (`1,000` changed)** | Native Bevy `Changed<Position>` | `0.0247 ms` | `0.0371 ms` | `3.73B ent/s` | `0.25 ns` |
| **`1%` (`1,000` changed)** | **C-ABI `AccessMode::CHANGED`** | **`0.0646 ms`** | `0.0771 ms` | **`1.52B ent/s`** | **`0.65 ns`** |
| **`50%` (`50,000` changed)** | Native Bevy `Changed<Position>` | `0.0412 ms` | `0.0512 ms` | `2.41B ent/s` | `0.41 ns` |
| **`50%` (`50,000` changed)** | **C-ABI `AccessMode::CHANGED`** | `0.5374 ms` | `0.6884 ms` | `182M ent/s` | `5.37 ns` |
| **`100%` (`100,000` changed)** | Native Bevy `Changed<Position>` | `0.0714 ms` | `0.0761 ms` | `1.45B ent/s` | `0.71 ns` |
| **`100%` (`100,000` changed)** | **C-ABI `AccessMode::CHANGED`** | **`0.1132 ms`** | `0.2021 ms` | **`854M ent/s`** | **`1.13 ns`** |

> At `1%` sparse mutations, completes a full `100,000`-entity scan + mutation pass in **`0.065 ms`** (`< 0.4%` of a 60 Hz frame), and at `50%` alternating odd/even rows (`50,000` distinct 1-row runs) stays well below `0.6 ms`. When `0%` of a table changed since `last_run`, the summary-tick check skips the entire table in $O(1)$.

### 2.3 Scenario 3: Deferred Command Burst (`10,000` `cmd_insert`s / Frame)
Compares native `Commands::entity(e).insert(Armor)` against pre-resolved `BamComponent` handle writes into per-system `CommandArena` (`MaybeUninit<u8>` bump slabs reused across frames with `0` steady-state heap allocations):

| Workload (`10,000` Writes/Frame) | Median Frame | p95 Frame | Throughput (`cmd/s`) | `ns / cmd` | Steady-State Heap Allocs |
|---|---:|---:|---:|---:|---:|
| Native Bevy `Commands::insert` | `0.1676 ms` | `0.1780 ms` | `59.4M cmd/s` | `16.76 ns` | `0` (reused `CommandQueue`) |
| **C-ABI `host_cmd_insert` (`CommandArena`)** | **`0.1964 ms`** | `0.2143 ms` | **`51.0M cmd/s`** | **`19.64 ns`** | **`0` (verified by gate)** |

### 2.4 Scenario 4: Batched Sparse-Set Query Iteration (`10,000` Entities)
Validates indirect row-pointer batching (`host_iter_column_rows_mutable` yielding whole archetype runs instead of 1-entity FFI chunks):

| Workload (`10,000` `SparseSet` Entities) | Median Frame | p95 Frame | Throughput (`ent/s`) | `ns / entity` | Chunks Emitted |
|---|---:|---:|---:|---:|---:|
| Native Bevy `Query<(&mut Pos, &mut SparseBuff)>` | `0.0119 ms` | `0.0131 ms` | `825M ent/s` | `1.19 ns` | N/A |
| **C-ABI Indirect Sparse Rows** | **`0.0267 ms`** | `0.0269 ms` | **`375M ent/s`** | **`2.67 ns`** | **`1` chunk (`10,000` rows)** |

### 2.5 Scenario 5: Batched `cmd_spawn` (4 Components) vs. Sequential Spawn+Insert
Spawns `2,500` 4-component entities (`Position`, `Velocity`, `Vitality`, `Armor`) per frame, comparing `host_cmd_spawn` (`EntityWorldMut::insert_by_ids` single target archetype transition) against `host_cmd_spawn_empty` + `4x host_cmd_insert` (4 intermediate table moves):

| Workload (`2,500` Spawns/Frame) | Median Frame | p95 Frame | Throughput (`ent/s`) | `ns / spawn` | Relative Speed |
|---|---:|---:|---:|---:|---:|
| Native Bevy 4-Component Tuple Bundle | `0.0563 ms` | `0.0716 ms` | `43.9M ent/s` | `22.52 ns` | `1.00x` (static bundle) |
| **C-ABI Batched `host_cmd_spawn` (4 comps)** | **`0.2432 ms`** | `0.2648 ms` | **`10.1M ent/s`** | **`97.28 ns`** | **`1.66x` faster than seq** |
| C-ABI Sequential `spawn_empty` + `4x insert` | `0.4047 ms` | `0.5419 ms` | `5.97M ent/s` | `161.88 ns` | `1.00x` |

---

## 3. Deterministic CI Regression Gate (`benches/deterministic_gate`)

Wall-clock microbenchmarks on shared cloud CI runners exhibit 15–40% noisy-neighbor variance. `benches/deterministic_gate` provides deterministic, zero-flake CI regression protection across platforms:
1. **Linux CI (`cfg(target_os = "linux")`):** Uses `iai-callgrind` (`Valgrind` Callgrind) to record deterministic CPU instruction counts, L1/LL cache hits, and estimated cycles across `bench_gate_dense_1k`, `bench_gate_changed_1pct`, `bench_gate_command_arena_1k`, `bench_gate_sparse_1k`, and `bench_gate_spawn4_256`.
2. **All Platforms (`CountingAlloc` + Step Invariants):** Wraps `std::alloc::System` in a global atomic `CountingAlloc` and asserts:
   - **`0` heap allocations** during steady-state dense chunk iteration (`run_dense_1k_gate`).
   - **`0` entities visited** on unmutated tables (`summary-tick skip) and exact `10` entities visited at `1%` mutation (`run_changed_1pct_gate`).
   - **`0` heap allocations** during steady-state `1,000`-command `host_cmd_insert` bursts (`run_command_arena_zero_alloc_gate`).
   - **`1` batched FFI chunk** emitted for `1,000` `SparseSet` entities (`run_sparse_batching_gate`).
   - **`256` 4-component entities** materialized in one deferred flush (`run_batched_spawn4_gate`).

---

## 4. Full 34-Archetype Permutation Matrix (`benches/permutation_bench`)

Beyond single-query microbenchmarks, `permutation_bench` evaluates a **34-archetype nested spatial combat simulation** (`1,024` units × `48` emitters = `49,152` pair evaluations/frame, and `3,072` × `96` = `294,912` pairs/frame in the high-scale workload) across **5 execution architectures × 5 FFI access patterns × Debug/Release host & guest profiles**, guarded by a bit-accurate floating-point `OracleDigest`.

### 4.1 Mod Loading, Handshake & `dlclose` Lifecycle Summary

| Architecture | Host Profile | Mod Profile | Compatibility | Binary Size | Warm Handshake + 34-Arch Init | Unload / `dlclose` |
|---|---|---|---|---:|---:|---|
| **Pure Native Bevy** | `release` | `static` | OK | `0 KB` | `7.8 µs` | N/A |
| **Coupled Rust `dylib`** | `release` | `release` | Fragile (same `rustc` only) | `209 KB` | `134.1 µs` | **Unsafe (`SIGSEGV` on TLS/TypeId drop)** |
| **Coupled Rust `dylib`** | `release` | `debug` | **`ABI_LOCKED` (Fails)** | N/A | N/A | N/A |
| **C-ABI Rust `cdylib`** | `release` | `release` | **Decoupled** | `378 KB` | **`151.8 µs`** | **`0.4–65 µs` (Safe)** |
| **C-ABI Rust `cdylib`** | `release` | `debug` | **Decoupled** | `493 KB` | **`180.8 µs`** | **`0.6–80 µs` (Safe)** |
| **C-ABI ISO C99 Mod** | `release` | `release (-O3)` | **Decoupled** | **`33 KB`** | **`201.4 µs`** | **`~915 µs` (Safe)** |

### 4.2 Access Pattern Degradation Ladder (`49,152` Pairwise 3D Evaluations/Frame)

| Access Pattern | Architecture | Host / Mod | Median Frame | `ns / pair` | vs. Native Ceiling | 60 Hz Budget |
|---|---|---|---:|---:|---:|---:|
| **1. `ChunkOptimized` ($O(U+E)$ FFI)** | C-ABI C99 Mod | `release` / `-O3` | **`0.1856 ms`** | `3.78 ns` | **`1.00x`** | `1.1%` |
| **2. Pre-Cached SoA Baseline** | Pure Native Bevy | `release` / `static` | **`0.1863 ms`** | `3.79 ns` | **`1.00x`** | `1.1%` |
| **3. `ChunkOptimized` ($O(U+E)$ FFI)** | C-ABI Rust `cdylib` | `release` / `release` | **`0.2151 ms`** | `4.38 ns` | **`1.15x`** | `1.3%` |
| **4. Idiomatic Nested `Query`** | Pure Native Bevy | `release` / `static` | `0.2180 ms` | `4.43 ns` | `1.17x` | `1.3%` |
| **5. Debug Mod + Release Host** | C-ABI C99 / Rust | `release` / `debug` | `0.94–1.14 ms` | `19–23 ns` | `5.1x–6.1x` | `5.7%–6.8%` |
| **6. `EntityNestedVTable` ($O(U\times E)$)** | C-ABI C99 / Rust | `release` / `release` | `0.70–0.72 ms` | `14.3–14.6 ns` | `3.78x–3.85x` | `4.2%–4.3%` |
| **7. `NaiveChattyFfi` (~1M FFI calls)** | C-ABI C99 / Rust | `release` / `release` | `1.44–1.47 ms` | `29.3–29.9 ns` | `7.72x–7.88x` | `8.6%–8.8%` |

### Key Takeaways for Mod Authors
1. **Hoist FFI Calls to the Chunk Level:** Crossing `extern "C"` once per archetype table (`ChunkOptimized` / `Chunk::component_mut`) runs within `0–15%` of statically linked Bevy—and even faster on simple dense loops thanks to contiguous slice auto-vectorization.
2. **Use Batched `cmd_spawn` for Multi-Component Entities:** Spawning `K` components via `cmd_spawn` writes directly into the final Bevy archetype in one transition instead of `K` table relocations.
3. **Debug Mods Inside a Release Host:** Because the C-ABI decouples optimization profiles, modders can attach LLDB/GDB to an `-O0` guest `.cdylib`/`.dylib` inside a full-speed `--release` Bevy game at `< 7%` frame budget cost.
