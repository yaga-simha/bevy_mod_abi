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
- **`ModErrorPolicy::Quarantine` *(Default)*:** Disables only the faulting guest system via an atomic flag, discards uncommitted commands from that invocation, emits `Messages<ModSystemFaulted>`, and keeps the rest of the mod and game running. Quarantined systems can be re-enabled at runtime via `world.resource::<ModQuarantine>().reenable(mod_id, "system_name")`.
- **`ModErrorPolicy::Log`:** Logs the fault via `tracing::warn!`, emits `Messages<ModSystemFaulted>`, and lets the system run again on future ticks.
- **`ModErrorPolicy::ReportToBevy`:** Forwards a `ModSystemError` into Bevy's native `Result<(), BevyError>` error handler pipeline.

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
```

---

## 3. Built-In & Custom Game Schedules (`ModScheduleMap`)

Guest systems can target any built-in Bevy schedule (`ScheduleId::STARTUP`, `PRE_UPDATE`, `UPDATE`, `POST_UPDATE`, `FIXED_UPDATE`, `LAST`) or game-defined custom schedules (`id >= ScheduleId::CUSTOM_BASE` = `1024`):

```rust
use bevy_ecs::schedule::ScheduleLabel;

#[derive(ScheduleLabel, Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RollbackPhysicsTick;

const ROLLBACK_SCHEDULE_ID: ScheduleId = ScheduleId::custom(1024);

app.init_schedule(RollbackPhysicsTick);
app.register_mod_schedule(ROLLBACK_SCHEDULE_ID, RollbackPhysicsTick)?;
```

- **Late `Startup` Execution (`L-01`):** If a mod is loaded at runtime *after* Bevy's `Startup` schedule has already run (`StartupCompleted` resource), any `ScheduleId::STARTUP` systems in that mod execute once immediately upon load commit.
- **Scoped Named Ordering (`L-05`):** Every guest system automatically joins `ModSystemSet(mod_id)` and `ModNamedSystemSet { mod_name, system_name }`. Unqualified `.after("step_a")` orders within the same mod; `"other_mod::step_a"` orders across mods without global name collisions.
- **Main-Thread Affinity (`B-07`):** Guests with thread-affine state set `BAM_SYSTEM_FLAG_MAIN_THREAD` (`.main_thread()` in Rust), mapping to Bevy's `SystemStateFlags::NON_SEND`.

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
