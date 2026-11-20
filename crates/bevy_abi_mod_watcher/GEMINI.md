# Agent Guide: `bevy_abi_mod_watcher`

Crate guide for **`bevy_abi_mod_watcher`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — debounced filesystem hot-reload watcher for native Bevy mods.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_watcher` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Monitors mod directories recursively via `notify-debouncer-full`, validates `mod.toml` SemVer DAG constraints, and drives cascading reverse/forward topological reloads and unloads in Bevy `PreUpdate`.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` code.
- **Allowed Dependencies:** `bevy_abi_mod_host_sys`, `bevy_abi_mod_host`, `bevy_app`, `bevy_ecs`, `notify-debouncer-full`, `semver`, `tracing` (plus `bevy_abi_mod_{macros,sys}` in `[dev-dependencies]`).
- **Keep `notify` Isolated Here:** Filesystem watching (`notify-debouncer-full`) belongs strictly in this crate so `bevy_abi_mod_host` stays lean and headless.

---

## 3. Owned Surface (`src/lib.rs`, `src/watcher.rs`)
- **`ModHotReloadPlugin` & `ModHotReloadWatcher`:** Configures watched directories, debounce timing, and automatic cascading unload/reload ordering across dependent mods in `PreUpdate`.
- **Outcome History:** `ModHotReloadHistory`, `ModHotReloadOutcome`, and `ModHotReloadAction` track chronological load, reload, unload, and validation failure records as a Bevy resource.

---

## 4. Strict Engineering Rules
1. **No Watcher State Corruption:** Never leave half-reloaded dependency subgraphs; rely on `bevy_abi_mod_host`'s DAG loader and transactional rollback on init failure.
2. **Non-Blocking Bevy Loop:** Drain debounced filesystem events cleanly in `PreUpdate` without blocking Bevy frame execution on file-watcher I/O.
3. **Zero Non-Test Panics:** Handle watcher channel disconnects, missing manifests, and transient OS file-replace races via `tracing` + `ModHotReloadHistory` instead of `.unwrap()`.

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
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{host_sys,host}`, `bevy_{app,ecs}` (`0.20.1`), `notify-debouncer-full` (`0.7`), `semver`, `tracing`, and `[dev-dependencies]` (`bevy_abi_mod_{macros,sys}`) in `Cargo.toml`.
2. **Update Repository URL & Root Configs:** Update `repository` in `Cargo.toml` if moved to a standalone repo; copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
3. **Pairing with `cargo-bam`:** Keep reload triggers compatible with `cargo bam watch --out-dir` atomic `.sig`-before-dylib staging.
