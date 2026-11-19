# Bridges, Trust & `cargo-bam` Tooling

Beyond core ECS query and command primitives, `bevy_abi_mod` provides a standalone developer CLI (`cargo-bam`), declarative `mod.toml` dependency DAG resolution (`bevy_abi_mod_manifest`), optional cryptographic signature enforcement (`bevy_abi_mod_trust`), and optional Bevy `0.20+` subsystem bridges.

---

## 1. Developer CLI (`cargo-bam`)

Install `cargo-bam` from the workspace:

```bash
cargo install --path crates/cargo-bam
```

Common subcommands:
- **`cargo bam new <name> --lang <rust|c99|cpp17|zig>`:** Scaffolds a standalone guest mod project (with `mod.toml` and build configuration) ready for host loading.
- **`cargo bam build [--dir <dir>] [--release] [--out-dir <dir>] [--sign-key <key>]`:** Compiles Rust (`cargo`), ISO C99, C++17, or Zig (`zig build`) guests, optionally staging the binary, `mod.toml`, and detached `.sig` sidecar under `<out-dir>`.
- **`cargo bam export-sdk --out <dir>`** *(alias: `cargo bam header`)*: Writes `include/bevy_abi_mod.h` (C99), `bevy_abi_mod.hpp` (C++17), and `bevy_abi_mod.zig` (Zig) from `bevy_abi_mod_sdk_gen`.
- **`cargo bam check-mods <dir>`:** Parses every `mod.toml` under `<dir>`, validates SemVer compatibility against host ABI `0.3.0`, and prints the resolved topological load order (or cycle/version errors).
- **`cargo bam doctor [--dir <dir>] [--require-c]`:** Verifies the host ABI header parity and local Rust, C99, C++17, and Zig compiler toolchains.
- **`cargo bam keygen` / `sign` / `verify`:** Generates OpenSSH Ed25519 keypairs, creates detached `SSHSIG` `.sig` sidecars (`--key`, `--namespace`), and verifies shared libraries against pinned `.pub` keys.

---

## 2. Manifests (`mod.toml`) & Dependency DAG Loading

Multi-mod directories use `mod.toml` manifests parsed by zero-Bevy `#![forbid(unsafe_code)]` crate `bevy_abi_mod_manifest`:

```toml
[mod]
id = "combat_ai"
name = "Combat AI"
version = "0.2.0"
abi_version = "^0.3.0"
language = "rust"
entry = "target/release/libcombat_ai.dylib"

[dependencies]
core_stats = "^1.0"

[optional_dependencies]
hud_markers = ">=0.1"
```

Call `app.load_mods_dir("mods")` (or `app.load_mods_dir_with("mods", &options)` / `world.load_mods_dir(...)`) to resolve all `mod.toml` manifests in Kahn topological order, verify `abi_version` compatibility against `ABI 0.3.0`, and prevent unloading mods still required by active dependents.

---

## 3. Cryptographic Verification (`signature-verification`)

Enabling the `signature-verification` feature on `bevy_abi_mod` / `bevy_abi_mod_host` wires the pure-Rust `#![forbid(unsafe_code)]` engine in `bevy_abi_mod_trust` into the pre-`dlopen` load path:

- **Trusted Keys, CAs, & Revocation Lists (`ModTrustPolicy`):** Configure via `AbiModPlugin::with_trust_policy(policy)` or `set_mod_trust_policy(...)`. Supports pinned Ed25519 leaf keys, OpenSSH Root CA certificates, principal / `BamManifest::name` binding, direct revocations, binary `SSHKRL` (`PROTOCOL.krl`) lists, and signed KRLs with monotonic anti-rollback versions.
- **Supported Payloads:** Validates detached OpenSSH `SSHSIG` `.sig` sidecars, embedded `.bam_sig` sections, and custom binary `.bampack` archives **before** mapping native machine code (`dlopen` / `LoadLibraryW`), preventing untrusted static constructors from running.
- **Audit Component & Runtime Eviction:** Loaded mods carry a `VerifiedModSignature` component (signer fingerprint, CA fingerprint, key ID, principals, serial, namespace, binary SHA-256) on their `LoadedModInfo` entity. After updating a KRL or trust policy at runtime, call `app.evict_revoked_mods()` / `world.evict_revoked_mods()` to unload revoked live mods cleanly.

---

## 4. Optional Host Feature Flags

Enable only the subsystems your game exposes on `bevy_abi_mod` or `bevy_abi_mod_host`:

| Feature | Host API / Plugin | What It Enables |
|---|---|---|
| `strict-ffi-checks` | Scoped handle poisoning | Debug/validation poisoning of `BamLoadCtx`, `BamSystemCtx`, `BamIter`, and `BamCommands` headers on return |
| `diagnostics` | `bevy_diagnostic` metrics | Per-mod and aggregate `cpu_time_us`, `fault_count`, and `commands_queued` telemetry |
| `signature-verification` | `ModTrustPolicy`, `VerifiedModSignature` | Pre-load OpenSSH Ed25519 `SSHSIG`, CA cert, `SSHKRL`, `.bam_sig`, and `.bampack` verification |
| `hot-reload-watcher` | `ModHotReloadPlugin` | Debounced filesystem watcher that reloads modified guest libraries automatically |
| `bridge-std-types` | `StdTypesBridgePlugin` | `Transform`, `GlobalTransform`, `Visibility`, `Name`, `Time<*>`, `ButtonInput<KeyCode>`, and `ButtonInput<MouseButton>` |
| `bridge-asset` | `ModAssetPlugin` | Sandboxed `mod://<namespace>/...` Bevy `AssetSource` and refcounted `FfiAssetHandle` |
| `bridge-state` | `ModStateExt`, `ModNextState<S>`, `ModStateSpec<S>` | Bevy `States` exposure, transition schedules, and `in_state` run conditions |
| `bridge-reflect` | `ModReflectPlugin` | Witness `DynamicStruct` synthesis, 6 `bevy_remote` BRP methods (`bevy_abi_mod/*`), and `bevy_settings` TOML sync |
| `bridge-bsn` | `ModBsnPlugin` | Bevy `0.20` `bsn!` scenes, `ScenePatch`, `cmd_spawn_scene`, and `Ready` observers |
| `sub-app-extract` | `ModSubAppPlugin<L>`, `ModWeslMaterialPlugin` | Cross-world `SubApp` extraction (`bevy_extract`) and data-driven `.wesl` / `ShaderBuffer` / 2D & 3D materials |
