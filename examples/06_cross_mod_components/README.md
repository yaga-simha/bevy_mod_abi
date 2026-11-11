# Example 6: Cross-Mod Dynamic Components

This example demonstrates decentralized, multi-mod ECS interoperability over the C-ABI:
**One loaded mod introduces a new component and spawns entities, while a completely independent loaded mod queries and mutates those same components without having introduced or spawned them.**

Neither mod depends on the other's compiled crate or binary, and the host application has **zero compile-time knowledge** of the custom component.

---

## Architectural Highlights

```text
┌──────────────────────────────────────────────────────────────────────────────────┐
│                             BEVY HOST APPLICATION                                │
│                                                                                  │
│   • Has ZERO compile-time knowledge of `EnergyShield`.                           │
│   • Manages dynamic `ModRegistry` and schedules mod systems in Bevy ECS.        │
└───────────────────────┬──────────────────────────────────┬───────────────────────┘
                        │                                  │
      Loads Mod A       │                                  │ Loads Mod B
      (ModId 1)         │                                  │ (ModId 2)
                        ▼                                  ▼
┌───────────────────────────────────┐    ┌─────────────────────────────────────────┐
│  Mod A: `mod_shield_emitter`      │    │  Mod B: `mod_overcharge_siphon`         │
│  (The Introducer / Spawner)       │    │  (The Consumer / Siphoner)              │
├───────────────────────────────────┤    ├─────────────────────────────────────────┤
│ • Introduces `EnergyShield` via   │    │ • Did NOT register `EnergyShield`.      │
│   `register_runtime_component`.   │    │ • Did NOT spawn any entities.           │
│ • Allocates entity IDs & attaches │    │ • Declares query for `"EnergyShield"`   │
│   `EnergyShield` via deferred     │    │   by name (`ComponentAccessMode::WRITE`)│
│   `GuestCommandQueue`.            │    │ • Siphons energy (-12.0 MW) directly    │
│ • Recharges shields (+5.0 MW/tick)│    │   from the archetype table column at    │
│   and modulates frequency.        │    │   hardware speed with zero copies.      │
└───────────────────────────────────┘    └─────────────────────────────────────────┘
```

---

## The Mod Interaction Flow

1. **Phase 1: Engine Initialization**
   The host initializes Bevy with `AbiModPlugin`. No shield components or types are registered in the host Rust binary.

2. **Phase 2: Loading Mod A (`mod_shield_emitter`)**
   In `bevy_abi_mod_init`:
   - Mod A calls `register_runtime_component::<EnergyShield>(host_api, host_context, c"EnergyShield")`.
   - The host runtime allocates a Bevy `ComponentId` and registers the descriptor in `ModRegistry`.
   - Mod A registers `shield_spawner_system` (which queues deferred entity spawns) and `shield_recharge_system`.

3. **Phase 3: Loading Mod B (`mod_overcharge_siphon`)**
   In `bevy_abi_mod_init`:
   - Mod B does **not** introduce any component.
   - Mod B specifies a query descriptor: `ComponentAccessDesc { name: c"EnergyShield".as_ptr(), mode: ComponentAccessMode::WRITE }`.
   - The host's dynamic system builder queries `ModRegistry`, resolves `EnergyShield`'s `ComponentId` and 12-byte stride, and binds Mod B's `shield_siphon_system` to Bevy's schedule.

4. **Phase 4: Execution & Data Sharing**
   - **Tick 1**: Mod A's spawner creates 3 drone entities with `EnergyShield` (`100.0`, `80.0`, and `60.0` MW).
   - **Tick 2+**:
     - Mod A recharges shields by `+5.0 MW`.
     - Mod B queries the table column, reads the drones spawned by Mod A, and siphons `-12.0 MW`.
     - The net change (`-7.0 MW`) is reflected in the host world without data races or memory corruption.

---

## Running the Example

```bash
cargo run -p example_06_cross_mod_components
```

To run the automated integration test:

```bash
cargo test -p example_06_cross_mod_components
```
