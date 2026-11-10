# Example 1: Minimal Handshake Proof of Concept

This example demonstrates the bare minimum handshake between a Bevy host engine and a native guest mod (`.cdylib`) over a zero-serialization C-ABI.

Both host and guest implementations are concise (<100 LOC each), intentionally stripped of complexity to clearly illustrate:
1. Initializing Bevy with `AbiModPlugin`.
2. Registering a component with `#[derive(Component, ModExposable)]`.
3. Compiling and dynamically loading a native guest mod `.cdylib`.
4. Mirrored VTable symbol lookup across the C boundary.
5. Zero-copy columnar chunk iteration using `GuestChunkIter`.
6. Safe mod unloading via atomic tombstones (`app.unload_mod(mod_id)`).

---

## Architecture Overview

```text
┌────────────────────────────────────────────────────────┐
│               HOST ENGINE (Bevy ECS v0.20+)            │
│                                                        │
│   #[derive(Component, ModExposable)]                   │
│   pub struct Position { pub x: f32, ... }              │
│                                                        │
│   app.add_plugins(AbiModPlugin::default());            │
│   app.register_mod_component::<Position>();            │
│   app.load_mod_tracked("libminimal_guest_mod.dylib");  │
└───────────────────────────┬────────────────────────────┘
                            │
            ════════════════╪════════════════
            STABLE C-ABI    │ (abi_contract)
            ════════════════╪════════════════
                            │
┌───────────────────────────┴────────────────────────────┐
│            GUEST MOD (.cdylib, Zero Bevy)              │
│                                                        │
│   #[repr(C)]                                           │
│   pub struct PositionVTable { size, get_x, set_x, ... }│
│                                                        │
│   wrap_system_callback!(fn c_nudge calls nudge_system);│
│   export_mod!(mod_init, mod_shutdown);                 │
└────────────────────────────────────────────────────────┘
```

### Why VTables?
Because Rust does not guarantee a stable ABI across compiler versions or compiler flags, sharing native Rust struct definitions across dynamic library boundaries causes memory corruption. The procedural macro `#[derive(ModExposable)]` creates a stable `#[repr(C)]` VTable with explicit byte strides and function pointers (`get_x`, `set_x`). The guest mod resolves this VTable dynamically via `get_proc_address(c"Position_vtable".as_ptr())`.

### Columnar Chunk Iteration
Bevy stores entity components in dense contiguous table columns. During execution, the host provides a `*mut RawChunkIter`. The guest wraps this in `GuestChunkIter::new(iter, api)` to access the column pointer via `column_mutable(0)` and mutate coordinates in-place without memory allocation or serialization.

---

## Running the Example

### Option A: Using Make
```bash
make example-1
```

### Option B: Using Cargo
```bash
# Build guest mod
cargo build --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml --release

# Run host application
cargo run --manifest-path examples/01_minimal_handshake/Cargo.toml
```

*(Note: The host application also features a built-in JIT compilation helper that automatically compiles the guest mod if the dynamic library is not yet present).*

---

## Expected Output
```text
=== Example 1: Minimal Handshake PoC ===
[HOST] Loaded mod ModId(1) from .../minimal_guest_mod.dylib

[HOST] Initial entity positions:
  Entity 0: (0.0, 0.0, 0.0)
  Entity 1: (10.0, 20.0, 30.0)
  Entity 2: (100.0, 200.0, 300.0)

[HOST] Running 3 Bevy update ticks...
  Tick 1 completed.
  Tick 2 completed.
  Tick 3 completed.

[HOST] Entity positions after 3 ticks:
  Entity 0: (3.0, 6.0, 9.0)
  Entity 1: (13.0, 26.0, 39.0)
  Entity 2: (103.0, 206.0, 309.0)

[HOST] Successfully unloaded mod. Handshake verification PASSED!
```
