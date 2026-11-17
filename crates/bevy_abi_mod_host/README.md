# `bevy_abi_mod_host`

Safe (`#![forbid(unsafe_code)]`) Bevy ECS (`v0.20+`) integration crate for [`bevy_abi_mod`](../bevy_abi_mod).

Provides the idiomatic Bevy plugin, extension traits, schedule/set bindings, and lifecycle messages for loading, executing, quarantining, hot-reloading, and unloading native C-ABI mods (`ABI 0.2.0`):

- **`AbiModPlugin`**: Initializes [`ModRegistry`], [`ModHost`], [`ModQuarantine`], [`ModScheduleMap`], [`ModVisibleSets`], [`WorldDropGuard`], and `Messages<ModLoaded>` / `Messages<ModUnloaded>` / `Messages<ModSystemFaulted>`.
- **`ModAppExt` & `ModWorldExt`**: Exposes Bevy components (`ComponentOptions`), singleton resources (`register_mod_resource`), buffered messages (`register_mod_message`), triggerable `Event` / `EntityEvent` types, custom schedules (`ScheduleId::custom`), and `"host::<name>"` visible `SystemSet`s.
- **`ModCommandsExt`**: Queues deferred mod loads, reloads, and unloads safely via Bevy `Commands`.
- **`ModQuarantine`**: Supports runtime inspection and `reenable(mod_id, system_name)` recovery for quarantined guest systems.

All `unsafe` FFI internals, Bevy `System` bridging, `HostIter` chunk iteration, `CommandArena` buffers, and `libloading` calls live in [`bevy_abi_mod_host_sys`](../bevy_abi_mod_host_sys).
