# Agent Guide: `bevy_abi_mod_reflect`

Crate guide for **`bevy_abi_mod_reflect`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — dynamic `bevy_reflect`, Bevy Remote Protocol (BRP), `bevy-settings` TOML, persistent save-game archives, and live telemetry.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_reflect` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Bridges modded witness and declarative `BamFieldDesc` schemas into Bevy `0.20.1` `AppTypeRegistry`, BRP (`bevy_remote`), `bevy-settings`, versioned save-game archives with asset handle round-tripping, and non-blocking `SystemTelemetryDigest64` telemetry streams.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` code.
- **Allowed Dependencies:** `bevy_abi_mod_{sys,host_sys,host}` (`host_sys` with `bridge-reflect`), `bevy_app` (`bevy_reflect`), `bevy_asset`, `bevy_ecs` (`bevy_reflect`), `bevy_platform`, `bevy_reflect`, `bevy_remote`, `bevy-settings`, `serde`, `serde_json`, `toml` (plus `bevy_abi_mod_macros` in `[dev-dependencies]`).

---

## 3. Owned Surface
- **Dynamic Reflection & Settings (`reflect.rs`, `codec.rs`):** `ModReflectPlugin`, `ModReflectAppExt`, `ModReflectWorldExt` — registers `DynamicStruct`, `ReflectFromPtr`, `ReflectComponent`, and `ReflectResource` from `ModComponent` witnesses or declarative `BamFieldDesc` schemas (including raw C99/C++17/Zig types), installs 6 `bevy_abi_mod/*` JSON-RPC BRP methods alongside `world.*`, and syncs mod settings via `bevy-settings` TOML.
- **Save Games (`savegame.rs`, `codec.rs`):** `ModSaveGamePlugin`, `ModSaveAppExt`, `ModSaveWorldExt`, `ModSaveArchive` (versioned JSON and checksummed `BAMSAVE1` binary), `ModSaveOptions` (`EphemeralHandleBehavior`), and recovery policies (`StrictRequireMods`, `StripMissingModComponents`, `MigrateOnLoad` via `ModComponentMigrator`). Uses unwrapped `TypedReflectSerializer` <-> `TypedReflectDeserializer` and Bevy `0.20.1` `HandleSerializeProcessor` / `HandleDeserializeProcessor` / `LoadFromPath` (without `.to_dynamic()` degradation) to round-trip `mod://` asset handles.
- **Live Telemetry (`telemetry.rs`):** `ModTelemetryPlugin`, `ModTelemetryRemoteExt`, `ModTelemetryWorldExt`, 4 `bam/*` BRP methods (`bam/list_mods`, `bam/system_metrics`, `bam/component_schemas`, `bam/hot_reload`), `ModBudgetTelemetry` ingestion, and non-blocking 64-byte `SystemTelemetryDigest64` frames over `AF_UNIX` or loopback TCP.

---

## 4. Strict Engineering Rules
1. **Keep Shared Codec Unified (`codec.rs`):** Both `reflect.rs` and `savegame.rs` MUST share `codec.rs` and use consistent unwrapped `TypedReflectSerializer` / `TypedReflectDeserializer` framing; NEVER convert reflected values with `.to_dynamic()` prior to `HandleSerializeProcessor`.
2. **Headless Save/Load Resilience:** Handle worlds without `AssetServer` cleanly via typed `ModSaveLoadError::Serialization` instead of panicking.
3. **Zero Non-Test Panics:** Maintain zero `.unwrap()` / `.expect()` calls in `src/`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,host_sys,host}`, `bevy_*` (`0.20.1`), `serde`, `serde_json`, `toml`, and `[dev-dependencies]` (`bevy_abi_mod_macros`) in `Cargo.toml`.
2. **Preserve `bridge-reflect` Feature Alignment:** Keep `bevy_abi_mod_host_sys` `features = ["bridge-reflect"]` enabled in `[dependencies]` and `EphemeralHandleBehavior` re-exported cleanly at root, `savegame`, and `prelude`.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
