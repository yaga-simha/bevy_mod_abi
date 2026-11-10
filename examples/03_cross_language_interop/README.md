# Example 3: Cross-Language Interoperability

This example demonstrates the true power of **Zero Shared Rust ABI Assumptions**: a pure **ISO C99 dynamic library** and an independent **Rust `.cdylib`** executing concurrently within the **exact same Bevy ECS schedule** on the **exact same entities**.

```text
┌────────────────────────────────────────────────────────────────────────┐
│                        HOST GAME (Bevy Engine)                         │
│  Dense Archetype Storage: Entity [0..N] in Contiguous Columns          │
│  - Position Column:  [Pos 0, Pos 1, Pos 2, ...]                        │
│  - Velocity Column:  [Vel 0, Vel 1, Vel 2, ...]                        │
│  - ColorRgba Column: [Col 0, Col 1, Col 2, ...]                        │
└───────────────────────────────────┬────────────────────────────────────┘
                                    │
                  ══════════════════╪══════════════════
                  Strict ISO C-ABI  │ (bevy_abi_mod.h)
                  ══════════════════╪══════════════════
                                    │
         ┌──────────────────────────┴──────────────────────────┐
         │                                                     │
         ▼                                                     ▼
┌───────────────────────────────┐     ┌───────────────────────────────┐
│         C99 Guest Mod         │     │        Rust Guest Mod         │
│       (`c_mod/mod.c`)         │     │     (`rust_mod/src/lib.rs`)   │
│                               │     │                               │
│ • Pure ISO C99 standard       │     │ • Independent Rust compiler   │
│ • Compiled with clang / gcc   │     │ • Zero Bevy dependency        │
│ • Updates Position via        │     │ • Mutates ColorRgba based     │
│   Velocity:                   │     │   on Position:                │
│   pos += vel * 0.1            │     │   color.r = f(pos.x), etc.    │
│ • Calls iter_mark_changed     │     │ • Calls chunk_iter.mark_changed│
└───────────────────────────────┘     └───────────────────────────────┘
```

---

## Architectural Principles

1. **Zero Shared Rust ABI**:
   The host is compiled with Bevy ECS. The C99 mod is compiled with `clang` or `gcc` linking only `libc`. The Rust mod is compiled independently with its own Rust toolchain and `guest_sdk`.
2. **Zero Cross-Allocator Crossover**:
   Memory allocated on the host is freed on the host. Memory allocated on the guest is freed on the guest. Dynamic systems operate directly on Bevy's dense archetype table memory via raw column pointers (`iter_column_mutable`, `iter_column_readonly`) and explicit strides.
3. **Automatic Concurrency & Data Race Freedom**:
   The C99 mod writes `Position` and reads `Velocity`. The Rust mod reads `Position` and writes `ColorRgba`. Bevy inspects the declared `ComponentAccessDesc` sets and guarantees safe ordered execution without locks or data races.
4. **Change Detection Propagation**:
   Both languages call `iter_mark_changed(iter, col, row)`. Host observers querying `Or<(Changed<Position>, Changed<ColorRgba>)>` immediately react to mutations regardless of which language performed them.

---

## Running the Example

### Option A: Using Make
```bash
make example-3
```

### Option B: Using Direct Commands
```bash
# Compile C99 guest mod
make -C examples/03_cross_language_interop/c_mod

# Compile Rust guest mod
cargo build --manifest-path examples/03_cross_language_interop/rust_mod/Cargo.toml --release

# Run host application
cargo run --manifest-path examples/03_cross_language_interop/Cargo.toml
```

*(Note: The host application also features a built-in JIT compilation helper that automatically compiles both mods if their dynamic libraries are missing).*

---

## Sample Output
```text
==================================================================
  bevy_abi_mod: Example 3 — Cross-Language Interoperability       
  C99 Guest Mod + Rust Guest Mod Running Concurrently in Bevy ECS 
==================================================================

[HOST] C99 Mod Library : .../c_mod/libc_mod.dylib
[HOST] Rust Mod Library: .../rust_mod/target/release/librust_mod.dylib

[HOST] Spawning 3 test drones with initial coordinates:
[HOST] Loading C99 guest mod...
[HOST] Loaded C99 mod (ModId: ModId(1))
[HOST] Loading Rust guest mod...
[HOST] Loaded Rust mod (ModId: ModId(2))

--- Commencing Simulation Ticks ---

>> Tick 1
  Drone Entity(0v1) | Pos: (  1.00,   0.50,   0.00) [C99] | Vel: (10.0,  5.0) | Color: RGBA(0.15, 0.22, 0.30, 1.00) [Rust]
  Drone Entity(1v1) | Pos: (  4.80,  -1.20,   0.00) [C99] | Vel: (-2.0,  8.0) | Color: RGBA(0.34, 0.20, 0.30, 1.00) [Rust]
  Drone Entity(2v1) | Pos: ( -9.60,  14.70,   0.00) [C99] | Vel: ( 4.0, -3.0) | Color: RGBA(0.58, 0.93, 0.30, 1.00) [Rust]
...
>> Tick 5
  Drone Entity(0v1) | Pos: (  5.00,   2.50,   0.00) [C99] | Vel: (10.0,  5.0) | Color: RGBA(0.35, 0.33, 0.30, 1.00) [Rust]
  Drone Entity(1v1) | Pos: (  4.00,   2.00,   0.00) [C99] | Vel: (-2.0,  8.0) | Color: RGBA(0.30, 0.30, 0.30, 1.00) [Rust]
  Drone Entity(2v1) | Pos: ( -8.00,  13.50,   0.00) [C99] | Vel: ( 4.0, -3.0) | Color: RGBA(0.50, 0.88, 0.30, 1.00) [Rust]

[HOST] Verified cooperative assertions for Drone e0: Pos (5.00, 2.50, 0.00), Color (0.35, 0.33, 0.30)
[HOST] Unloading both mods (Atomic Tombstone + Shutdown Hook)...
[HOST] Executing 1 post-unload tick (expecting 0 mutations):

==================================================================
  SUCCESS: Cross-language interop verified with zero data races!  
==================================================================
```
