# Example 01: Minimal Handshake

**Concept taught:** Minimal ABI `0.3.0` handshake (`BamManifest` + per-`World` `BamInstance`), shadow-copy loading, lifecycle `Messages`, and typed column mutation.

---

## What It Shows

1. **Host component registration (`bevy_mod_abi`):** Exposes `Position` (`#[mod_component(name = "example::Position")]`) via `app.register_mod_component::<Position>(ComponentOptions::new().with_raw_insert().with_default())`.
2. **Guest manifest & zero-boilerplate derive (`bevy_abi_mod_guest`):** Derives `#[derive(..., AbiPod, GuestComponent)]` with `#[guest_component(name = "example::Position")]` and uses `export_mod!(plugin: MinimalHandshakeMod)` to export `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` without process-global state or `bevy` dependencies.
3. **Unified system builder & typed queries:** Registers `position_nudge` via `app.system(c"position_nudge").query_typed::<&mut Position>().schedule(ScheduleId::UPDATE).build(|ctx| ctx.single_query::<&mut Position>().for_each(...))` with zero positional column indices.
4. **Shadow-copy load & deterministic unload:** Loads with `LoadOptions { shadow_copy: true, strict: true, .. }`, observes `Messages<ModLoaded>` / `Messages<ModUnloaded>`, runs 3 Bevy `Update` ticks, and unloads cleanly via `app.unload_mod(mod_id)`.

## Commands

```bash
# Build the standalone Rust guest .cdylib
cargo build --manifest-path examples/01_minimal_handshake/guest_mod/Cargo.toml --release

# Run the host binary
cargo run -p example_01_minimal_handshake
```
