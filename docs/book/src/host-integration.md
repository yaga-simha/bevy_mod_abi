# Host Integration, Budgets & Schedules (`bevy_mod_abi` / `bevy_abi_mod_host`)

**`bevy_mod_abi`** (canonical host facade; legacy `bevy_abi_mod` is a deprecated compatibility shim) and the slim headless `bevy_abi_mod_host` crate expose a 100% safe (`#![forbid(unsafe_code)]`) host API across `App`, `&mut World`, and deferred `Commands`. `bevy_abi_mod_host` pulls in **zero** `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` dependencies, so headless dedicated servers pay only for `bevy_ecs` + `bevy_app`.

---

## 1. Adding `AbiModPlugin`, Error Policies & `FallbackErrorHandler`

```rust
use bevy_mod_abi::prelude::*;
use bevy_app::App;

let mut app = App::new();
app.add_plugins(AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine));
```

Available `ModErrorPolicy` modes when a guest system returns a non-`OK` `FfiResult` (including `PANIC_CAUGHT` or faults reported via `host_report_guest_fault`):
- **`ModErrorPolicy::Quarantine` *(Default)*:** Quarantines the faulting guest system (`enabled = false`), discards uncommitted commands from that invocation, logs via `tracing::warn!`, emits `Messages<ModSystemFaulted>` (including `mod_namespace`, `generation`, and `last_error`), and returns `Ok(())` so the rest of the mod and game keep running. Quarantined systems can be re-enabled at runtime via `world.resource::<ModQuarantine>().reenable(mod_id, "system_name")`.
- **`ModErrorPolicy::Log`:** Quarantines the faulting guest system (`enabled = false`), discards uncommitted commands, escalates logging to `tracing::error!`, emits `Messages<ModSystemFaulted>`, and returns `Ok(())` (also re-enableable via `ModQuarantine::reenable`).
- **`ModErrorPolicy::ReportToBevy`:** Quarantines the faulting guest system, logs via `tracing::error!`, emits `Messages<ModSystemFaulted>`, and forwards `ModSystemError` into Bevy `0.20`'s `FallbackErrorHandler` with `ErrorContext::System` (guest observer faults likewise discard uncommitted commands and route to `FallbackErrorHandler` with `ErrorContext::Observer`).
- **`ModErrorPolicy::DelegateToBevyFallback`:** Discards uncommitted commands, emits `Messages<ModSystemFaulted>`, and delegates a contextualized `BevyError` directly to Bevy's world `FallbackErrorHandler` without auto-quarantining the system.

---

## 2. Per-Mod Resource Quotas & Frame-Time Watchdogs (`ModBudgetPolicy`)

Protect live frame budgets and prevent runaway mods from exhausting memory or stalling ticks via `ModBudgetPolicy`, `ModBudgetLimits`, `ModBudgetTelemetry`, and `ModBudgetExt`:

```rust
use bevy_mod_abi::prelude::*;
use bevy_app::App;

let mut app = App::new();
app.add_plugins(
    AbiModPlugin::with_error_policy(ModErrorPolicy::Quarantine)
        .with_budget_policy(
            ModBudgetPolicy::default().with_default_limits(ModBudgetLimits {
                max_arena_bytes: Some(2 * 1024 * 1024),   // 2 MiB CommandArena cap -> CAPACITY_EXCEEDED
                max_spawns_per_frame: Some(500),           // Atomic per-mod frame cap across all 5 spawn commands
                max_frame_time_us: Some(4_000),            // 4 ms sliding-window p99 watchdog -> auto-quarantine
                ..Default::default()
            }),
        ),
);
```

- **Zero Steady-State Overhead:** Arena caps rewind safely via `ArenaWatermark` on partial batch overflow, spawn caps use lock-free atomic counters reset in `First`, and per-system `p50_us`/`p99_us` + `summary_tick` skip metrics flow into `ModBudgetTelemetry`, `Messages<ModBudgetBreach>`, optional `diagnostics` counters, and `ModTelemetryPlugin` (`bam/system_metrics` BRP + 64B telemetry stream).
- **Per-Mod Overrides:** Override limits at runtime by `ModId` or namespace (`per_mod_overrides` / `per_name_overrides`) via `app.set_mod_budget_policy(...)` / `world.set_mod_budget_policy(...)`.

---

## 3. Exposing Host Components & Resources (`ComponentOptions`)

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

Additional `ComponentOptions` modifiers include `.opaque()` (disables raw byte insertion so mutations go only through VTables or `Default`) and `.with_summary_tick()` (`BAM_STORAGE_FLAG_SUMMARY_TICK` per-table change-tick acceleration, validated against `StorageType::Table`).

Because `CommandArena` flushes deferred spawns, inserts, relation mutations, and removals through Bevy's `EntityWorldMut` (`insert_by_ids` / `remove_by_id`), host `ComponentHooks` (`on_add` / `on_insert` / `on_replace` / `on_remove`), `#[require(...)]` Required Components, and `ChildOf` / `Children` relationships automatically stay consistent when mutated by guest mods. On unload, the host cleans up mod-scoped and `LoadedModInfo` entities via Bevy `0.20.1`'s `despawn_all_where` (including `Allow<Disabled>` entities).

---

## 4. Built-In, Custom & Hot-Rewireable Schedules (`ModScheduleMap`, `ModVisibleSets`, `ModScheduleSetRegistry`)

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
- **Scoped Named Ordering, `.chain_weak()` & Host Sets:** Every guest system automatically joins `ModSystemSet::Mod(mod_id)` and `ModNamedSystemSet { mod_name, system_name, generation }`. Unqualified `.after(c"step_a")` orders within the same mod; `c"other_mod::step_a"` orders across mods; `c"host::input"` orders `.in_set()`, `.before()`, or `.after()` relative to host-exported `ModVisibleSets`. Guests using ABI `0.3.0`'s decoupled `configure_schedule_set` / `mount_system_in_schedule` execute through `ModScheduleSetRewirePass` supporting `.chain()`, `.chain_weak()`, `.before_weak()`, `.after_weak()`, cycle detection (`CYCLE_DETECTED`), and sub-millisecond schedule-only hot-rewiring without re-initializing `GuestSystem` kernels.
- **Main-Thread Affinity:** Guests with thread-affine state set `BAM_SYSTEM_FLAG_MAIN_THREAD` (`.main_thread()` in Rust), mapping to Bevy's `SystemStateFlags::NON_SEND`.

---

## 5. Runtime Loading from Exclusive Systems or `Commands`

All load and unload operations require exclusive `World` access (`ModWorldExt` on `&mut World` or deferred `ModCommandsExt` on `Commands`), making concurrent `dlclose` data races impossible at compile time. For multi-mod directories with `mod.toml` SemVer dependencies, call `app.load_mods_dir("mods")` or `app.load_mods_dir_with("mods", &options)` (see [Bridges, Render, Reflect, Trust & `cargo-bam`](./bridges-and-tooling.md)).

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
