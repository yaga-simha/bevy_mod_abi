# Example 2: Multi-System Pipeline

This example demonstrates a production-grade ECS pipeline running inside a native dynamic mod (`.cdylib`) with zero Bevy dependencies.

It progresses beyond the single-system proof-of-concept in Example 1 by showcasing:
1. **Multi-Component Queries**: Reading and writing multiple components simultaneously (`Position`, `Velocity`, `Health`).
2. **All 4 Query Access Modes**:
   - `ComponentAccessMode::READ` (`&T` read-only access)
   - `ComponentAccessMode::WRITE` (`&mut T` mutable access)
   - `ComponentAccessMode::FILTER_ONLY` (Archetype inclusion filter, equivalent to Bevy's `With<T>`)
   - `ComponentAccessMode::WITHOUT` (Archetype exclusion filter, equivalent to Bevy's `Without<T>`)
3. **Deferred Commands Queue**:
   - Synchronously allocating entity IDs via `GuestCommandQueue::spawn_empty()`
   - Safely attaching typed component values via `GuestCommandQueue::insert_value()`
   - Deferred entity destruction via `GuestCommandQueue::despawn()`
4. **Change Detection Integration**:
   - Marking column and row mutations via `chunk_iter.mark_changed(col, row)`
   - Seamless detection by host systems querying `Changed<Position>` and `Changed<Health>`
5. **Per-Mod State Encapsulation**:
   - Managing guest heap state (`PipelineState`) safely through `SystemDescriptor.user_data` without cross-runtime allocator leakage.

---

## The Three Interacting Systems

```text
┌─────────────────────────────────────────────────────────────┐
│                     GUEST MOD (.cdylib)                     │
│                                                             │
│   ┌─────────────────────────────────────────────────────┐   │
│   │ System 1: KinematicsMovementSystem                  │   │
│   │ Access: Position (WRITE, col 0), Velocity (READ, 1) │   │
│   │ Action: Moves all entities that have Velocity.      │   │
│   └──────────────────────────┬──────────────────────────┘   │
│                              ▼                              │
│   ┌─────────────────────────────────────────────────────┐   │
│   │ System 2: FleetLifecycleSystem                      │   │
│   │ Access: Health (WRITE, 0), Position (READ, 1),      │   │
│   │         PlayerTag (WITHOUT, 2)                      │   │
│   │ Action: • Decrements minion drone health.           │   │
│   │         • Despawns dead drones via commands.despawn.│   │
│   │         • Spawns reinforcements via spawn_empty +   │   │
│   │           insert_value.                             │   │
│   └──────────────────────────┬──────────────────────────┘   │
│                              ▼                              │
│   ┌─────────────────────────────────────────────────────┐   │
│   │ System 3: PlayerRegenSystem                         │   │
│   │ Access: Health (WRITE, 0),                          │   │
│   │         PlayerTag (FILTER_ONLY, 1)                  │   │
│   │ Action: Heals only entities tagged with PlayerTag.  │   │
│   └─────────────────────────────────────────────────────┘   │
└──────────────────────────────┬──────────────────────────────┘
                               │
                               ▼
┌─────────────────────────────────────────────────────────────┐
│                    HOST GAME (Bevy Engine)                  │
│                                                             │
│   • monitor_motion_changes (Query<..., Changed<Position>>)  │
│   • monitor_health_changes (Query<..., Changed<Health>>)    │
│   • Asserts invariants: player healed, drone despawned,     │
│     reinforcement drone spawned deferred.                   │
└─────────────────────────────────────────────────────────────┘
```

---

## Running the Example

### Option A: Using Make
```bash
make example-2
```

### Option B: Using Cargo
```bash
# Build guest mod
cargo build --manifest-path examples/02_multi_system_pipeline/guest_mod/Cargo.toml --release

# Run host application
cargo run --manifest-path examples/02_multi_system_pipeline/Cargo.toml
```

---

## Key Invariants Verified
- **Invariant A**: Player Ship is shielded from minion attrition by `WITHOUT PlayerTag` and receives health regeneration from `PlayerRegenSystem` (`FILTER_ONLY PlayerTag`).
- **Invariant B**: Low-health Minion Drone Beta reaches 0 HP and is cleanly despawned deferred via `commands.despawn(entity)`.
- **Invariant C**: Reinforcement Drone Gamma is dynamically spawned on tick 1 via `commands.spawn_empty()` and populated with `Position`, `Velocity`, `Health`, and `DroneTag` via `commands.insert_value()`.
