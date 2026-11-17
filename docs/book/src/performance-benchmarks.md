# Performance Tuning & Benchmarks

`bevy_abi_mod` is engineered so decoupled native mods run at native Bevy speed within 60 Hz (`16.67 ms`) and 120 Hz (`8.33 ms`) frame budgets. Full hardware baselines and commands live in [`docs/BENCHMARKS.md`](../../BENCHMARKS.md).

---

## 1. Five Golden Rules for Mod Performance

1. **Iterate by Chunk, Never by Chatty Per-Entity FFI Calls:**
   Crossing `extern "C"` once per Bevy archetype table (`Chunk::component_mut`, `Chunk::read_write`, or `iter_column_mutable` in C/C++/Zig) amortizes FFI dispatch over hundreds or thousands of contiguous entities and unlocks LLVM SIMD auto-vectorization (`0.15–0.33 ns/entity` at `100k` entities on Apple M4 Pro).
2. **Pre-Resolve `BamComponent` Handles Once in `init`:**
   Resolve every component and resource handle during `GuestPlugin::build` / `bevy_abi_mod_init`. Per-frame commands (`cmd_insert`, `cmd_spawn`) index `RegistrySnapshot` in $O(1)$ with zero UTF-8 checks or string hashing.
3. **Prefer `#[repr(C)]` `AbiPod` for High-Frequency Numeric Data:**
   Use `#[repr(C)]` `f32`/`u32` structs for transforms, velocities, physics state, and spatial sensors so guests operate on contiguous slices directly. Reserve VTable getters/setters for cold or infrequent metadata (`String` labels, variable-length `Vec<T>` inventories).
4. **Use Batched `cmd_spawn` Instead of `spawn_empty` + `N` `insert`s:**
   `GuestCommands::spawn(&entries)` (`host_cmd_spawn`) copies all `K` component payloads into the system's reusable 64 KiB `CommandArena` (`MaybeUninit<u8>`, `0` steady-state heap allocations) and inserts them via `EntityWorldMut::insert_by_ids` in **one** archetype transition (`~1.66x` faster than sequential spawn + 4 inserts).
5. **Leverage Untracked Mutable Slices When Conditional Writes Are Rare:**
   Calling `iter_column_mutable` marks the chunk's change ticks once per chunk (`P-04`). If a system inspects `100,000` entities and mutates only `< 1%` of them, use `column_mut_untracked` + `mark_changed_range(term, row, 1)` so downstream `Changed<T>` systems (`P-01`) skip whole unchanged tables in $O(1)$.

---

## 2. Reference Baselines (Apple M4 Pro, `rustc 1.98.1`, Release)

| Canonical Scenario (`X-03`) | Native Bevy | `bevy_abi_mod` (`ABI 0.2.0`) | Steady-State Heap Allocs |
|---|---:|---:|---:|
| **Dense Iterate (`100k` `Pos + Vel`)** | `0.0900 ms` (`0.90 ns/ent`) | **`0.0263 ms`** (`0.26 ns/ent`, SoA SIMD) | `0` |
| **`Changed<Position>` (`100k` @ `1%` mutated)** | `0.0247 ms` (`0.25 ns/ent`) | **`0.0646 ms`** (`0.65 ns/ent`, `P-01`) | `0` |
| **Command Burst (`10k` `cmd_insert`s/frame)** | `0.1676 ms` (`16.8 ns/cmd`) | **`0.1964 ms`** (`19.6 ns/cmd`, `CommandArena`) | `0` |
| **Sparse-Set Query (`10k` `SparseSet` entities)** | `0.0119 ms` (`1.19 ns/ent`) | **`0.0267 ms`** (`2.67 ns/ent`, `1` chunk `P-02`) | `0` |
| **Spawn-with-4 (`2,500` spawns/frame)** | `0.0563 ms` (static bundle) | **`0.2432 ms`** (`1.66x` faster than seq) | `0` |
| **34-Archetype Spatial Combat (`49,152` pairs)** | `0.1863 ms` (`1.00x`) | **`0.1856 ms` C99 (`1.00x`) / `0.2151 ms` Rust (`1.15x`)** | `0` |

---

## 3. Running Benchmarks & Regression Gates Locally

```bash
# Run all 5 canonical scenarios (builds Rust & C99 fixtures first)
cargo xtask bench

# Run quick mode locally
cargo bench -p bevy_abi_mod_host --bench chunk_throughput -- --quick

# Run deterministic zero-allocation & step-count / iai-callgrind gate
cargo bench --manifest-path benches/deterministic_gate/Cargo.toml
```
