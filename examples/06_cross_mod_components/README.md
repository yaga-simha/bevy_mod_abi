# Example 06: Cross-Mod Dynamic Components

**Concept taught:** Runtime dynamic component registration (`GuestApp::register_component`) and cross-mod component + ordering interoperability without host compile-time component types.

---

## What It Shows

1. **Runtime component introduction (`mod_shield_emitter`):** Mod A registers `"mod_shield_emitter::EnergyShield"` dynamically via `app.register_component::<EnergyShield>()` and spawns drone entities carrying it via `GuestCommands::spawn`. The host binary has no compile-time registration for `EnergyShield`.
2. **Cross-mod handle & UUID resolution (`mod_overcharge_siphon`):** Mod B declares `GuestComponent` with `NAME = c"mod_shield_emitter::EnergyShield"`, resolves Mod A's component handle and 128-bit `ComponentUuid` from `ModRegistry`, and mutates the same dense archetype columns.
3. **Cross-mod system ordering (`L-05`):** Mod B orders `shield_siphon_system` `.after(c"mod_shield_emitter::shield_recharge_system")` via `ModNamedSystemSet` so recharge and siphon stages execute deterministically every frame.

## Commands

```bash
# Build both standalone Rust guest .cdylibs
cargo build --manifest-path examples/06_cross_mod_components/mods/mod_shield_emitter/Cargo.toml --release
cargo build --manifest-path examples/06_cross_mod_components/mods/mod_overcharge_siphon/Cargo.toml --release

# Run the host binary
cargo run -p example_06_cross_mod_components

# Run the integration test
cargo test -p example_06_cross_mod_components
```
