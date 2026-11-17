# Host Integration & Schedules (`bevy_abi_mod` / `bevy_abi_mod_host`)

`bevy_abi_mod` exposes a 100% safe (`#![forbid(unsafe_code)]`) host API across `App`, `&mut World`, and deferred `Commands`.

---

## 1. Adding `AbiModPlugin` & Error Policies

```rust
use bevy_abi_mod::prelude::*;
use bevy_app::App;

let mut app = App::new();
app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
```

Available [`ModErrorPolicy`] modes when a guest system returns a non-`OK` `FfiResult` (including `PANIC_CAUGHT`):
- **`ModErrorPolicy::Quarantine` *(Default)*:** Quarantines the faulting guest system (`enabled = false`), discards uncommitted commands from that invocation, logs via `tracing::warn!`, emits `Messages<ModSystemFaulted>`, and returns `Ok(())` so the rest of the mod and game keep running. Quarantined systems can be re-enabled at runtime via `world.resource::<ModQuarantine>().reenable(mod_id, "system_name")`.
- **`ModErrorPolicy::Log`:** Quarantines the faulting guest system (`enabled = false`), discards uncommitted commands, escalates logging to `tracing::error!`, emits `Messages<ModSystemFaulted>`, and returns `Ok(())` (also re-enableable via `ModQuarantine::reenable`).
- **`ModErrorPolicy::ReportToBevy`:** Quarantines the faulting guest system, logs via `tracing::error!`, emits `Messages<ModSystemFaulted>`, and forwards `ModSystemError` via `Err(RunSystemError::from(...))` into Bevy's native error handler pipeline.

---

## 2. Exposing Host Components & Resources (`ComponentOptions`)

Derive `ModComponent` on any Bevy `Component` or `Resource` and register it with explicit capabilities:

```rust
// 1. #[repr(C)] POD component: allow raw byte inserts (`cmd_insert` / `cmd_spawn`) and `Default`
app.register_mod_component::<Position>(
    ComponentOptions::new().with_raw_insert().with_default(),
)?;

// 2. Opaque #[repr(Rust)] component with String/Vec fields + SparseSet + Immutable flags
app.register_mod_component::<UnitMetadata>(
    ComponentOptions::new()
        .with_default()
        .sparse_set()
        .immutable(),
)?;

// 3. Singleton Bevy Resource (`AccessMode::RESOURCE_READ` / `RESOURCE_WRITE`)
app.init_resource::<SimConfig>();
app.register_mod_resource::<SimConfig>()?;

// 4. Buffered POD Bevy Messages (`Messages<M>`) & Triggerable Events
app.register_mod_message::<DamageMessage>()?;
app.register_mod_event::<WaveStarted>()?;
app.register_mod_entity_event::<HitEntity>()?;
```

Because `CommandArena` flushes deferred spawns, inserts, and removals through Bevy's `EntityWorldMut` (`insert_by_ids` / `remove_by_id`), host `ComponentHooks` (`on_add` / `on_insert` / `on_replace` / `on_remove`), `#[require(...)]` Required Components, and `ChildOf` / `Children` relationships automatically stay consistent when mutated by guest mods.

---

## 3. Built-In & Custom Game Schedules (`ModScheduleMap` & `ModVisibleSets`)

Guest systems can target any built-in Bevy schedule (`ScheduleId::STARTUP`, `PRE_UPDATE`, `UPDATE`, `POST_UPDATE`, `FIXED_UPDATE`, `LAST`) or game-defined custom schedules (`id >= ScheduleId::CUSTOM_BASE` = `1024`), and constrain execution order relative to host-exported `SystemSet`s (`ModVisibleSets`) under Bevy `0.20+` schedule randomization:

```rust
use bevy_ecs::schedule::{ScheduleLabel, SystemSet};

#[derive(ScheduleLabel, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RollbackPhysicsTick;

#[derive(SystemSet, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HostInputSet;

const ROLLBACK_SCHEDULE_ID: ScheduleId = ScheduleId::custom(1024);

app.init_schedule(RollbackPhysicsTick);
app.register_mod_schedule(ROLLBACK_SCHEDULE_ID, RollbackPhysicsTick)?;
app.register_mod_visible_set("input", HostInputSet);
```

- **Late `Startup` Execution:** If a mod is loaded at runtime *after* Bevy's `Startup` schedule has already run (`StartupCompleted` resource), any `ScheduleId::STARTUP` systems in that mod execute once immediately upon load commit.
- **Scoped Named Ordering & Host Sets:** Every guest system automatically joins `ModSystemSet(mod_id)` and `ModNamedSystemSet { mod_name, system_name }`. Unqualified `.after("step_a")` orders within the same mod; `"other_mod::step_a"` orders across mods; `"host::input"` orders `.in_set()`, `.before()`, or `.after()` relative to host-exported `ModVisibleSets`.
- **Main-Thread Affinity:** Guests with thread-affine state set `BAM_SYSTEM_FLAG_MAIN_THREAD` (`.main_thread()` in Rust), mapping to Bevy's `SystemStateFlags::NON_SEND`.

---

## 4. Runtime Loading from Exclusive Systems or `Commands`

All load and unload operations require exclusive `World` access (`ModWorldExt` on `&mut World` or deferred `ModCommandsExt` on `Commands`), making concurrent `dlclose` data races impossible at compile time:

```rust
fn hot_swap_keybind_system(mut commands: Commands) {
    commands.load_mod_with(
        "mods/libcombat_v2.dylib",
        LoadOptions {
            shadow_copy: true,
            strict: true,
            ..Default::default()
        },
    );
}
```
