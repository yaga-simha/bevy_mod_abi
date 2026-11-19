# Example 02: Multi-System Pipeline

**Concept taught:** Multi-system guest pipelines using `app.system(...)` — typed tuple queries (`query_typed` with `With` / `Without`), system ordering (`.after(...)`), run conditions (`.run_if_fn(...)`), and batched single-archetype deferred commands (`GuestCommands::spawn`).

---

## What It Shows

1. **Declarative multi-system builder:** Registers `KinematicsMovementSystem`, `FleetLifecycleSystem` (`.after(c"KinematicsMovementSystem")`), and `PlayerRegenSystem` (`.after(c"FleetLifecycleSystem").run_if_fn(...)`).
2. **Typed tuple queries & archetype filters:** Uses `.query_typed::<(FfiEntity, &mut Health, &Position, Without<PlayerTag>)>()` with `ctx.single_query_and_commands` to apply drone attrition without damaging the player ship, and `.query_typed::<(&mut Health, With<PlayerTag>)>()` with `ctx.single_query` to regenerate only player entities.
3. **Handle-based batched spawning:** Resolves `BamComponent` handles once at `GuestPlugin::build` time and spawns reinforcement drones in one archetype transition via `GuestCommands::spawn(&[SpawnEntry::new(...), ...])` alongside `GuestCommands::despawn`.
4. **Change-tick propagation:** Contiguous SoA chunk writes (`&mut [Position]`, `&mut [Health]`) mark Bevy change ticks once per chunk so host `Changed<Position>` and `Changed<Health>` queries observe guest mutations.

## Commands

```bash
# Build the standalone Rust guest .cdylib
cargo build --manifest-path examples/02_multi_system_pipeline/guest_mod/Cargo.toml --release

# Run the host binary
cargo run -p example_02_multi_system_pipeline
```
