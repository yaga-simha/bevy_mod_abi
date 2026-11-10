# Comprehensive Benchmark Matrix: Native vs. Coupled `dylib` vs. Asymmetric C-ABI (`Debug` & `Release`)

This document presents the full cross-architecture and cross-profile empirical performance evaluation of **`bevy_abi_mod`**. Every permutation of execution model, access pattern, and build target (`dev`/`debug` vs. `release`/`-O3`) is benchmarked inside a live Bevy ECS `App` running a **34-archetype nested `Query` simulation**.

To reproduce all measurements and regenerate [`target/benchmark_results.json`](../target/benchmark_results.json):

```bash
make bench
# or directly:
cargo run --release -p permutation_bench
```

---

## 1. Benchmark Architecture & Complex Multi-Archetype Workload

Simple single-archetype `Position += Velocity` microbenchmarks mask the real costs of dynamic ECS modding: multi-table archetype switching, negative query filters (`Without<T>`), interior pointers into nested structs, VTable dispatch overhead, change-detection tick updates, and unoptimized cross-profile FFI boundaries.

### 1.1 Nested Compound Component Graph (`benches/shared_scenario`)

The benchmark world ([`benches/shared_scenario/src/lib.rs`](../benches/shared_scenario/src/lib.rs)) defines **15 ECS components** and nested `#[derive(ModExposable)]` sub-structs:
- **`Transform3D`**: contains **two nested `Vec3f` structs** (`position: Vec3f`, `forward: Vec3f`) plus `scale: f32` (accessed across C-ABI via `Transform3D_ptr_position` / `Transform3D_ptr_mut_position` + `Vec3fVTable`).
- **`Kinematics`**: contains a **nested `Vec3f` struct** (`velocity: Vec3f`) plus `drag: f32` and `max_speed: f32`.
- **`Vitality`**: `health`, `max_health`, `shield`, `energy` (`f32 × 4`).
- **`Resistance`**: `kinetic`, `thermal`, `ionic` (`f32 × 3`).
- **`AuraEmitter`**: `radius`, `force`, `thermal_dps`, `heal_hps`, `falloff` (`f32 × 5`).
- **`Faction`**: `team_id: u32`, `serial_id: u32`.
- **Archetype Fragmentation & Filter Components**: `CombatUnitTag`, `EmitterBeaconTag`, `FrozenFlag`, `ArmorPlating`, `StealthCloak`, `SquadLeader`, `Overcharged`, `PulseModulator`, and heap-allocated `TelemetryTag { label: String }`.

### 1.2 34-Archetype World Topology

`populate_scenario_world` deterministically populates **34+ distinct Bevy Archetype Tables**:
1. **16 Active Combat Unit Archetypes**: Every combination of optional `[ArmorPlating, StealthCloak, SquadLeader, Overcharged]` (`2^4 = 16` tables) carrying `(Transform3D, Kinematics, Vitality, Resistance, Faction, CombatUnitTag)` and `Without<EmitterBeaconTag>`, `Without<FrozenFlag>`.
2. **6 Active Aura Emitter Archetypes**: 6 distinct component combinations (`PulseModulator`, `Overcharged`, `StealthCloak`, `TelemetryTag(String)`) carrying `(Transform3D, AuraEmitter, Faction, EmitterBeaconTag)` and `Without<CombatUnitTag>`, `Without<FrozenFlag>`.
3. **12 Excluded Decoy & Frozen Archetypes**: Frozen units (`With<FrozenFlag>`), frozen emitters, static props, partial-component decoys, and dual-tagged entities that must be rejected at zero runtime cost by Bevy's archetype filter graph.

### 1.3 Pairwise Nested Query Workload & Mathematical Oracle

Every frame executes an outer-by-inner pairwise spatial field simulation (`native_nested_aura_combat_system`):
- **Standard Scale**: `1,024` active units (across 16 tables) × `48` active emitters (across 6 tables) = **`49,152` pair evaluations/frame** (`1,328` total entities).
- **Heavy Stress Scale**: `3,072` active units × `96` active emitters = **`294,912` pair evaluations/frame** (`3,552` total entities).
- **Oracle Verification**: Before recording any timing, `OracleDigest` runs 4 simulation frames across **every single permutation** (Pure Native Bevy, Coupled Rust `dylib`, C-ABI Rust `cdylib`, and C-ABI C99 across `Debug`, `Release`, and all 3 FFI access patterns) and asserts bit-accurate floating-point agreement (`< 1e-4` tolerance across all 1,024 units).

---

## 2. Dimension 1: Mod Loading, Handshake & Unload Matrix (`Debug` vs. `Release`)

Each sample copies the shared library to a fresh temporary file path so the OS dynamic loader (`dyld` / `ld.so`) performs genuine Mach-O/ELF mapping and symbol binding rather than returning a `< 1 µs` cached handle. `Init+Sch` isolates the `bevy_abi_mod` handshake (`get_proc_address` for 8 VTables + `register_multi_query_system`) and Bevy's 34-archetype `Schedule::initialize`.

| Architecture | Host Target | Mod Target | Compatibility | Binary Size | Cold OS `dlopen` | ABI Handshake + 34-Arch Schedule Init | Unload / `dlclose` | Lifecycle Notes |
|---|---|---|---|---:|---:|---:|---:|---|
| **Pure Native Bevy (No-Mod)** | `release` | `static` | **OK** | `0 KB` | `0.0 µs` | **`7.8 µs`** | `0.0 µs` | Statically linked into host binary |
| **Pure Native Bevy (No-Mod)** | `debug` | `static` | **OK** | `0 KB` | `0.0 µs` | **`128.2 µs`** | `0.0 µs` | Unoptimized Bevy schedule graph build |
| **Coupled Rust `dylib` Mod** | `release` | `release` | **OK (Fragile)** | `209.2 KB` | `~44–125 ms` | `134.1 µs` | *Unsafe to `dlclose`*\* | Requires identical `rustc` + flags + shared `dylib` |
| **Coupled Rust `dylib` Mod** | `release` | `debug` | **`ABI_LOCKED`** | `N/A` | `N/A` | `N/A` | `N/A` | **Fails**: `StableCrateId` & `TypeId` hashes mismatch |
| **Coupled Rust `dylib` Mod** | `debug` | `release` | **`ABI_LOCKED`** | `N/A` | `N/A` | `N/A` | `N/A` | **Fails**: `StableCrateId` & `TypeId` hashes mismatch |
| **Coupled Rust `dylib` Mod** | `debug` | `debug` | **OK (Fragile)** | `478.4 KB` | `~51–236 ms` | `493.4 µs` | *Unsafe to `dlclose`*\* | Cannot safely unmap TLS/TypeId drop glue |
| **C-ABI Rust `cdylib` Mod** | `release` | `release` | **OK (Decoupled)** | `378.2 KB` | `~48–137 ms` | **`151.8 µs`** | **`0.4–65 µs`** | Resolves 8 VTables + registers multi-query system |
| **C-ABI Rust `cdylib` Mod** | `release` | `debug` | **OK (Decoupled)** | `493.3 KB` | `~46–180 ms` | **`180.8 µs`** | **`0.6–80 µs`** | **Works seamlessly across Release Host + Debug Mod** |
| **C-ABI Rust `cdylib` Mod** | `debug` | `release` | **OK (Decoupled)** | `378.2 KB` | `~13.6–48 ms` | **`257.0 µs`** | **`2.0 µs`** | **Works seamlessly across Debug Host + Release Mod** |
| **C-ABI Rust `cdylib` Mod** | `debug` | `debug` | **OK (Decoupled)** | `493.3 KB` | `~12.1–55 ms` | **`263.2 µs`** | **`2.5 µs`** | Full debug symbols on both sides |
| **C-ABI C99 Mod** | `release` | `release (-O3)` | **OK (Decoupled)** | **`33.2 KB`** | `~42–198 ms` | **`201.4 µs`** | `915–1077 µs` | Tiny 33 KB pure C99 binary; clean `dlclose` |
| **C-ABI C99 Mod** | `release` | `debug (-O0)` | **OK (Decoupled)** | **`34.3 KB`** | `~45–262 ms` | **`144.6 µs`** | `915.9 µs` | Debug C99 mod inside Release Bevy Host |
| **C-ABI C99 Mod** | `debug` | `release (-O3)` | **OK (Decoupled)** | **`33.2 KB`** | `~6.8–44 ms` | **`215.9 µs`** | `876.2 µs` | Optimized C99 mod inside Debug Bevy Host |
| **C-ABI C99 Mod** | `debug` | `debug (-O0)` | **OK (Decoupled)** | **`34.3 KB`** | `~8.3–43 ms` | **`304.5 µs`** | `877.5 µs` | Full `-O0 -g` source-level C debugging |

> **\*Critical Finding on Coupled Rust `dylib` Unloading (`SIGSEGV` Hazard):** Calling `dlclose` (`drop(Library)`) on a Coupled Rust `dylib` after registering native Rust systems into Bevy causes a `SIGSEGV` when TLS destructors or leftover `TypeId` vtables execute during shutdown because they point into unmapped `.text`/`.rodata` pages. By contrast, `bevy_abi_mod` C-ABI mods keep all Rust trait vtables (`DynamicCSystem`) inside the **Host** binary and cleanly disable/evict guest callbacks before `dlclose`.
>
> **Note on Cold `dlopen` Latency on macOS:** First-time loading of a newly written unsigned `.dylib` path on macOS incurs `amfid`/signature verification (`~12–200 ms`), whereas warm/cached `dlopen` + full `bevy_abi_mod` 8-VTable handshake + 34-archetype Bevy schedule initialization completes in **`130–200 µs` (`0.15 ms`)**.

---

## 3. Dimension 2: The Complete Performance Degradation Ladder (`1.00x` -> `66.47x`)

All rows below execute the **exact same 34-archetype nested spatial combat simulation** (`1,024` active units across 16 tables × `48` active emitters across 6 tables = **`49,152` pair evaluations/frame**), ranked from **True Native Hardware Ceiling (`1.00x`)** down to the worst unoptimized/chatty FFI permutation (**`66.47x` slower**):

| Rank | Architecture | Access Pattern | Host Target | Mod Target | Median Frame | `ns / pair` | Slowdown vs True Native | 60 FPS Budget (`16.67 ms`) |
|---:|---|---|---|---|---:|---:|---:|---:|
| **1** | **Coupled Rust `dylib`** | Pre-Cached SoA + Shared Rust `Query` | `release` | `release` | **`0.1823 ms`** | `3.71 ns` | **`0.98x`** | `1.1%` |
| **2** | **C-ABI C99 Mod** | **`ChunkOptimized` ($O(U+E)$ VTable)** | `release` | `release (-O3)` | **`0.1856 ms`** | `3.78 ns` | **`1.00x`** | `1.1%` |
| **3** | **Pure Native Bevy (No-Mod)** | **Pre-Cached SoA + Inlined `Query` (Baseline)** | `release` | `static` | **`0.1863 ms`** | `3.79 ns` | **`1.00x`** | `1.1%` |
| **4** | **C-ABI Rust `cdylib`** | **`ChunkOptimized` ($O(U+E)$ VTable)** | `release` | `release` | **`0.2151 ms`** | `4.38 ns` | **`1.15x`** | `1.3%` |
| **5** | **Pure Native Bevy (No-Mod)** | Direct Idiomatic Nested `Query` (6 Inner Tables) | `release` | `static` | **`0.2180 ms`** | `4.43 ns` | **`1.17x`** | `1.3%` |
| **6** | **Coupled Rust `dylib`** | Direct Idiomatic Nested `Query` (Shared Rust ABI) | `release` | `release` | **`0.2195 ms`** | `4.47 ns` | **`1.18x`** | `1.3%` |
| **7** | **C-ABI C99 Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | **`debug`** | `release (-O3)` | `0.3911 ms` | `7.96 ns` | **`2.10x`** | `2.3%` |
| **8** | **Dual C-ABI (Rust + C99)** | 2x Serialized Multi-Query Systems / Frame | `release` | `rel + rel (-O3)` | `0.4092 ms` | `8.32 ns` | **`2.20x`** | `2.5%` |
| **9** | **C-ABI Rust `cdylib`** | `ChunkOptimized` ($O(U+E)$ VTable) | **`debug`** | `release` | `0.4365 ms` | `8.88 ns` | **`2.34x`** | `2.6%` |
| **10** | **C-ABI C99 Mod** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | `release (-O3)` | `0.7037 ms` | `14.32 ns` | **`3.78x`** | `4.2%` |
| **11** | **C-ABI Rust `cdylib`** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | `release` | `0.7173 ms` | `14.59 ns` | **`3.85x`** | `4.3%` |
| **12** | **Dual C-ABI (Rust + C99)** | 2x Serialized Multi-Query Systems / Frame | **`debug`** | `rel + rel (-O3)` | `0.7855 ms` | `15.98 ns` | **`4.22x`** | `4.7%` |
| **13** | **C-ABI C99 Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | `release` | **`debug (-O0)`** | `0.9442 ms` | `19.21 ns` | **`5.07x`** | `5.7%` |
| **14** | **C-ABI Rust `cdylib`** | `ChunkOptimized` ($O(U+E)$ VTable) | `release` | **`debug`** | `1.1412 ms` | `23.22 ns` | **`6.12x`** | `6.8%` |
| **15** | **C-ABI C99 Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | **`debug`** | **`debug (-O0)`** | `1.2091 ms` | `24.60 ns` | **`6.49x`** | `7.3%` |
| **16** | **C-ABI Rust `cdylib`** | `ChunkOptimized` ($O(U+E)$ VTable) | **`debug`** | **`debug`** | `1.4070 ms` | `28.63 ns` | **`7.55x`** | `8.4%` |
| **17** | **Pure Native Bevy (No-Mod)** | Pre-Cached SoA + Inlined `Query` | **`debug`** | `static` | `1.4198 ms` | `28.89 ns` | **`7.62x`** | `8.5%` |
| **18** | **C-ABI Rust `cdylib`** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | `release` | `1.4385 ms` | `29.27 ns` | **`7.72x`** | `8.6%` |
| **19** | **C-ABI C99 Mod** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | `release (-O3)` | `1.4683 ms` | `29.87 ns` | **`7.88x`** | `8.8%` |
| **20** | **Coupled Rust `dylib`** | Pre-Cached SoA + Shared Rust `Query` | **`debug`** | **`debug`** | `1.4925 ms` | `30.36 ns` | **`8.01x`** | `9.0%` |
| **21** | **C-ABI C99 Mod** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | **`debug (-O0)`** | `1.6307 ms` | `33.18 ns` | **`8.75x`** | `9.8%` |
| **22** | **C-ABI Rust `cdylib`** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | **`debug`** | `2.1775 ms` | `44.30 ns` | **`11.69x`** | `13.1%` |
| **23** | **C-ABI C99 Mod** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | **`debug (-O0)`** | `2.3618 ms` | `48.05 ns` | **`12.67x`** | `14.2%` |
| **24** | **C-ABI Rust `cdylib`** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | **`debug`** | `2.8670 ms` | `58.33 ns` | **`15.39x`** | `17.2%` |
| **25** | **C-ABI C99 Mod** | `EntityNestedVTable` ($O(U \times E)$ Getters) | **`debug`** | `release (-O3)` | `7.0383 ms` | `143.19 ns` | **`37.77x`** | `42.2%` |
| **26** | **C-ABI Rust `cdylib`** | `EntityNestedVTable` ($O(U \times E)$ Getters) | **`debug`** | `release` | `7.0608 ms` | `143.65 ns` | **`37.89x`** | `42.4%` |
| **27** | **Pure Native Bevy (No-Mod)** | Direct Idiomatic Nested `Query` (6 Inner Tables) | **`debug`** | `static` | `8.2560 ms` | `167.97 ns` | **`44.31x`** | `49.5%` |
| **28** | **Coupled Rust `dylib`** | Direct Idiomatic Nested `Query` (Shared Rust ABI) | **`debug`** | **`debug`** | `8.3163 ms` | `169.20 ns` | **`44.63x`** | `49.9%` |
| **29** | **C-ABI C99 Mod** | `EntityNestedVTable` ($O(U \times E)$ Getters) | **`debug`** | **`debug (-O0)`** | `8.4458 ms` | `171.83 ns` | **`45.33x`** | `50.7%` |
| **30** | **C-ABI Rust `cdylib`** | `EntityNestedVTable` ($O(U \times E)$ Getters) | **`debug`** | **`debug`** | `9.0414 ms` | `183.95 ns` | **`48.52x`** | `54.2%` |
| **31** | **C-ABI C99 Mod** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | **`debug`** | `release (-O3)` | `10.4757 ms` | `213.13 ns` | **`56.22x`** | `62.9%` |
| **32** | **C-ABI Rust `cdylib`** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | **`debug`** | `release` | `10.5230 ms` | `214.09 ns` | **`56.47x`** | `63.1%` |
| **33** | **C-ABI C99 Mod** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | **`debug`** | **`debug (-O0)`** | `12.1467 ms` | `247.12 ns` | **`65.19x`** | `72.9%` |
| **34** | **C-ABI Rust `cdylib`** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | **`debug`** | **`debug`** | **`12.3858 ms`** | `251.99 ns` | **`66.47x`** | **`74.3%`** |

---

## 4. Dimension 3: High-Scale Stress Tier (`294,912` Pairs/Frame across 34 Archetypes)

Scaling `6x` higher (`3,072` units × `96` emitters = **`294,912` pairwise 3D combat evaluations per frame**) in a `Release` Host confirms linear scaling with zero per-frame heap allocations:

| Architecture | Access Pattern | Host Target | Mod Target | Median Frame | `ns / pair` | Slowdown vs Native Ceiling | 60 FPS Budget |
|---|---|---|---|---:|---:|---:|---:|
| **C-ABI C99 Mod** | `ChunkOptimized` ($O(U+E)$ VTable) | `release` | `release (-O3)` | **`1.0900 ms`** | `3.70 ns` | **`0.93x`** | `6.5%` |
| **Coupled Rust `dylib`** | Pre-Cached SoA + Shared Rust `Query` | `release` | `release` | **`1.1627 ms`** | `3.94 ns` | **`0.99x`** | `7.0%` |
| **Pure Native Bevy (No-Mod)** | Pre-Cached SoA + Inlined `Query` | `release` | `static` | **`1.1732 ms`** | `3.98 ns` | **`1.00x`** | `7.0%` |
| **C-ABI Rust `cdylib`** | `ChunkOptimized` ($O(U+E)$ VTable) | `release` | `release` | **`1.2470 ms`** | `4.23 ns` | **`1.06x`** | `7.5%` |
| **Coupled Rust `dylib`** | Direct Nested `Query` (Shared Rust ABI) | `release` | `release` | **`1.3150 ms`** | `4.46 ns` | **`1.12x`** | `7.9%` |
| **Pure Native Bevy (No-Mod)** | Direct Nested `Query` (6 Inner Tables) | `release` | `static` | **`1.3245 ms`** | `4.49 ns` | **`1.13x`** | `7.9%` |
| **Dual C-ABI (Rust + C99)** | 2x Serialized Multi-Query Systems / Frame | `release` | `rel + rel (-O3)` | `2.4856 ms` | `8.43 ns` | **`2.12x`** | `14.9%` |
| **C-ABI C99 Mod** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | `release (-O3)` | `3.8776 ms` | `13.15 ns` | **`3.31x`** | `23.3%` |
| **C-ABI Rust `cdylib`** | `EntityNestedVTable` ($O(U \times E)$ Getters) | `release` | `release` | `3.9062 ms` | `13.25 ns` | **`3.33x`** | `23.4%` |
| **C-ABI Rust `cdylib`** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | `release` | `8.1648 ms` | `27.69 ns` | **`6.96x`** | `49.0%` |
| **C-ABI C99 Mod** | `NaiveChattyFfi` ($O(U \times E)$ R/W + Tick) | `release` | `release (-O3)` | `8.1995 ms` | `27.80 ns` | **`6.99x`** | `49.2%` |

---

## 5. Architectural Takeaways: Why Performance Degrades Step-by-Step

1. **Tier 0 (`1.00x` — `0.186 ms` / `3.79 ns/pair`): What True Native Performance Actually Is**
   - When both Host and Guest are compiled in `release` (`-O3`), **Chunk-Optimized C-ABI (`0.186 ms` in C99, `0.215 ms` in Rust `cdylib`) matches Pure Native Bevy (`0.186 ms` SoA-cached, `0.218 ms` idiomatic nested `Query`) within `0%–15%`**.
   - Even better: Chunk-Optimized C-ABI (`0.186 ms` C99 / `0.215 ms` Rust) **beats idiomatic Pure Native Bevy nested `Query` (`0.218 ms`)** because hoisting the 6 fragmented inner emitter tables once per frame eliminates `1,024 × 6 = 6,144` archetype table switches per frame and reduces VTable crossings from $O(U \times E)$ (`~600,000` calls) to $O(U + E)$ (`~29,000` calls).

2. **Tier 1 (`1.01x`–`1.18x`): Coupled Rust `dylib` Gives Zero Speed Advantage Over Chunk-Optimized C-ABI — At Massive Safety/Portability Cost**
   - Coupled Rust `dylib` (`0.182 ms` SoA-cached / `0.220 ms` direct nested `Query`) runs at virtually the same speed as `bevy_abi_mod` Chunk-Optimized C-ABI (`0.186 ms`–`0.215 ms`), yet:
     - **Cannot load across `Debug`/`Release` profiles** (`ABI_LOCKED` due to `StableCrateId` & `TypeId` hash changes).
     - **Cannot load across `rustc` patch releases**.
     - **Cannot safely `dlclose`** without risking `SIGSEGV` during TLS/drop teardown.

3. **Tier 2 (`2.10x`–`2.34x`): Running a `Debug` Host with a `Release` C-ABI Mod**
   - A `Debug` Bevy Host running a `Release` C-ABI mod takes only **`0.39–0.44 ms` (`2.1x`–`2.3x`)**, whereas Pure Native Bevy in `Debug` takes **`8.26 ms` (`44.3x` slower)**!
   - Why? Because `ChunkOptimized` crosses into the Host VTable only $O(U + E)$ times, while `99%` of the pairwise math executes inside the `-O3` / `--release` mod. Conversely, if you compile the **Mod** in `Debug` inside a `Release` Host (`0.94–1.14 ms`, **`5.1x`–`6.1x`**), mod developers get full source-levelLLDB/GDB stepping while still keeping `93%+` of their 60 FPS frame budget!

4. **Tier 3 (`3.78x`–`3.85x`): Per-Pair Indirect VTable Calls (`EntityNestedVTable`)**
   - Calling `iter_reset` per unit and invoking indirect `extern "C"` VTable getters inside the `U × E` inner loop prevents LLVM/Clang from inlining field reads or SIMD-vectorizing across emitters, costing **`~3.8x`** (`0.71 ms` vs `0.19 ms`) in `Release`.
   - When the **Host** is compiled in `Debug`, those `~600,000` host-side VTable getter calls become unoptimized Rust functions, jumping to **`7.04 ms` (`37.8x` slower)**!

5. **Tier 4 (`7.72x`–`66.47x`): Naive Chatty FFI + Per-Pair Tick Marking (`NaiveChattyFfi`)**
   - Reading and writing component fields across VTable pointers on **every pair interaction** plus calling `iter_mark_changed` inside the inner loop forces pointer-aliased memory roundtrips and ~1M indirect FFI calls/frame:
     - `Release` Host + `Release` Mod: **`1.44–1.47 ms` (`7.7x`–`7.9x` slower)**
     - `Release` Host + `Debug` Mod: **`2.36–2.87 ms` (`12.7x`–`15.4x` slower)**
     - `Debug` Host + `Release` Mod: **`10.48–10.52 ms` (`56.2x`–`56.5x` slower)**
     - `Debug` Host + `Debug` Mod: **`12.15–12.39 ms` (`65.2x`–`66.5x` slower — consuming `74.3%` of the entire 60 FPS budget!)**
