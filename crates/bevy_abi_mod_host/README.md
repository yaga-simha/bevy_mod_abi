# `bevy_abi_mod_host`

Safe (`#![forbid(unsafe_code)]`) Bevy ECS (`v0.20+`) integration crate for [`bevy_abi_mod`](../bevy_abi_mod).

Provides the idiomatic Bevy plugin, extension traits, schedule/set bindings, and lifecycle messages for loading, executing, quarantining, hot-reloading, and unloading native C-ABI mods (`ABI 0.2.0`):

- **`AbiModPlugin`**: Initializes [`ModRegistry`], [`ModHost`], [`ModQuarantine`], [`ModScheduleMap`], [`ModVisibleSets`], [`WorldDropGuard`], and `Messages<ModLoaded>` / `Messages<ModUnloaded>` / `Messages<ModSystemFaulted>`.
- **`ModAppExt` & `ModWorldExt`**: Exposes Bevy components (`ComponentOptions`), singleton resources (`register_mod_resource`), buffered messages (`register_mod_message`), triggerable `Event` / `EntityEvent` types, custom schedules (`ScheduleId::custom`), and `"host::<name>"` visible `SystemSet`s.
- **`ModCommandsExt`**: Queues deferred mod loads, reloads, and unloads safely via Bevy `Commands`.
- **`ModQuarantine`**: Supports runtime inspection and `reenable(mod_id, system_name)` recovery for quarantined guest systems.
- **`signature-verification` (optional feature)**: Enforces load-time cryptographic verification (`ModTrustPolicy`, `AbiModPlugin::with_trust_policy`, `ModWorldExt::set_mod_trust_policy`) of detached `<mod>.sig` OpenSSH `SSHSIG` signatures against Ed25519 CA certificates or pinned public keys and OpenSSH `SSHKRL` / signed KRLs (`KrlBuilder`, anti-rollback, `ModWorldExt::evict_revoked_mods`) prior to `dlopen` / `LoadLibraryW`, attaching `VerifiedModSignature` to `LoadedModInfo` entities. Use `cargo xtask sign <keygen|cert|file|krl|verify>` or `ssh-keygen -Y`/`-k`/`-Q` to manage certs, KRLs, and mod signatures.

All `unsafe` FFI internals, Bevy `System` bridging, `HostIter` chunk iteration, `CommandArena` buffers, and `libloading` calls live in [`bevy_abi_mod_host_sys`](../bevy_abi_mod_host_sys).
