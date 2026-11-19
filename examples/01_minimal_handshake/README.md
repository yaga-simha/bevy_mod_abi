# Example 01: Minimal Handshake

**Concept taught:** Minimal ABI `0.3.0` handshake (`BamManifest` + per-`World` `BamInstance`), shadow-copy loading, lifecycle `Messages`, and typed column mutation.

---

## What It Shows

1. **Host component registration (`bevy_abi_mod`):** Exposes `Position` (`#[mod_component(name = "example::Position")]`) via `app.register_mod_component::<Position>(ComponentOptions::new().with_raw_insert().with_default())`.
2. **Guest manifest & per-World lifecycle (`bevy_abi_mod_guest`):** `export_mod!(plugin: MinimalHandshakeMod)` exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` without process-global state or `bevy` dependencies.
3. **Unified system builder & UUID-checked views:** Registers `position_nudge` via `app.system(c"position_nudge").query(|q| q.write::<Position>()).build(...)` and mutates `Chunk::component_mut::<Position>(0)` in place.
4. **Shadow-copy load & deterministic unload:** Loads with `LoadOptions { shadow_copy: true, strict: true, .. }`, observes `Messages<ModLoaded>` / `Messages<ModUnloaded>`, runs 3 Bevy `Update` ticks, and unloads cleanly via `app.unload_mod(mod_id)`.

## Commands

```bash
# Build the standalone Rust guest .cdylib
cargo build --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml --release

# Run the host binary
cargo run -p example_01_minimal_handshake
```
