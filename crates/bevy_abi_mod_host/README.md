# `bevy_abi_mod_host`

Safe (`#![forbid(unsafe_code)]`) Bevy ECS (`v0.20+`) integration crate for [`bevy_abi_mod`](../bevy_abi_mod).

Provides the idiomatic Bevy plugin, extension traits, schedule/set bindings, `mod.toml` SemVer DAG loading, and lifecycle messages for loading, executing, quarantining, hot-reloading, and unloading native C-ABI mods (`ABI 0.3.0`):

- **`AbiModPlugin`**: Initializes [`ModRegistry`], [`ModHost`], [`ModQuarantine`], [`ModScheduleMap`], [`ModVisibleSets`], [`WorldDropGuard`], and `Messages<ModLoaded>` / `Messages<ModUnloaded>` / `Messages<ModSystemFaulted>`.
- **`ModAppExt` & `ModWorldExt`**: Exposes Bevy components (`ComponentOptions`), singleton resources (`register_mod_resource`), buffered POD and non-POD messages (`register_mod_message`, `register_mod_non_pod_message`), triggerable `Event` / `EntityEvent` types, custom schedules (`ScheduleId::custom`), `"host::<name>"` visible `SystemSet`s, and `load_mods_dir` / `ModDependencyGraph` DAG resolution via [`bevy_abi_mod_manifest`](../bevy_abi_mod_manifest).
- **`ModCommandsExt`**: Queues deferred mod loads, reloads, and unloads safely via Bevy `Commands`.
- **`ModQuarantine`**: Supports runtime inspection and `reenable(mod_id, system_name)` recovery for quarantined guest systems.
- **Optional Feature Integrations**: `strict-ffi-checks`, `diagnostics`, `signature-verification` (`ModTrustPolicy` + `evict_revoked_mods` backed by [`bevy_abi_mod_trust`](../bevy_abi_mod_trust)), `hot-reload-watcher` (`ModHotReloadPlugin`), `bridge-std-types`, `bridge-asset`, `bridge-state`, `bridge-reflect`, `bridge-bsn`, and `sub-app-extract`. See [`crates/bevy_abi_mod/README.md`](../bevy_abi_mod/README.md) for feature details.

All `unsafe` FFI internals, Bevy `System` bridging, `HostIter` chunk iteration, `CommandArena` buffers, and `libloading` calls live in [`bevy_abi_mod_host_sys`](../bevy_abi_mod_host_sys).
