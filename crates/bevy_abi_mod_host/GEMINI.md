# Agent Guide: `bevy_abi_mod_host`

Crate guide for **`bevy_abi_mod_host`** (`v0.3.0`, ABI `0x0003_0000`, Bevy `0.20.1+`) — the safe, **slim headless** Bevy ECS host plugin crate.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_host` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Provides the ergonomic, safe Bevy ECS host API (`AbiModPlugin`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModBudgetPolicy`, `ModUnloadHooks`) decoupled from rendering, reflection, scenes, assets, and filesystem watchers.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` blocks anywhere; all FFI/`libloading` internals live in `bevy_abi_mod_host_sys`.
- **Headless Isolation (CRITICAL):** **NEVER** depend on `bevy_render`, `bevy_reflect`, `bevy_scene`, `bevy_asset`, or `notify` (even transitively through feature flags). Allowed Bevy deps are strictly `bevy_ecs`, `bevy_app`, `bevy_platform`, and optional `bevy_diagnostic` (`diagnostics`).

---

## 3. Owned Surface & Feature Flags
- **Features:** `strict-ffi-checks`, `test-utils`, `diagnostics` (enables `bevy_diagnostic`), `signature-verification` (delegates to `bevy_abi_mod_trust` via `host_sys`).
- **Core Plugin & Extension Traits (`plugin.rs`, `app_ext.rs`, `world_ext.rs`, `commands_ext.rs`, `events.rs`):**
  - `AbiModPlugin`, `ModAppExt`, `ModWorldExt`, `ModCommandsExt`, `ModQuarantine`, `ModVisibleSets`, `ModUnloadHooks`, and `prelude`.
  - `ModWorldExt::despawn_loaded_mod_entities` uses Bevy `0.20.1` `world.despawn_all_where::<&LoadedModInfo, Allow<Disabled>>(...)`.
  - `register_mod_component` validates `BAM_STORAGE_FLAG_SUMMARY_TICK` / `ComponentOptions::with_summary_tick()` against `ComponentInfo::summary_tick()` and `StorageType::Table`.
- **Quotas, Diagnostics & DAG Loading (`budgets.rs`, `diagnostics.rs`, `mod_dag.rs`, `schedule_sets.rs`):** `ModBudgetPolicy`, `ModBudgetLimits`, `ModBudgetBreach`, `ModBudgetTelemetry`, `ModBudgetExt`, `load_mods_dir` / `ModDependencyGraph` (`bevy_abi_mod_manifest`), and Bevy diagnostic counters.

---

## 4. Strict Engineering Rules
1. **Zero Non-Test Panics & Keep `src/` Lean:** Maintain zero non-test `.unwrap()` / `.expect()` and zero `#[allow(...)]` suppressions in `src/`.
2. **Idiomatic Bevy ECS (`0.20+`):** Prefer `Res<T>`/`&T` over mutable borrows, narrow queries with `With`/`Without`, use `despawn_all_where`, `.chain_weak()`, and `On<E>` observers cleanly.
3. **Preserve Companion Decoupling:** Extensions requiring rendering, reflection, assets, scenes, or file watching belong in `bevy_abi_mod_render`, `bevy_abi_mod_reflect`, `bevy_abi_mod_bridges`, or `bevy_abi_mod_watcher` hooked via `ModUnloadHooks` — never here.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo clippy --all-targets --no-default-features -- -D warnings
cargo tree --all-features | grep -E "bevy_render|bevy_reflect|bevy_scene|bevy_asset|notify"   # MUST print NOTHING
BAM_FIXTURE_DIR="/path/to/fixtures" cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Decouple Root-Path Tests in `Cargo.toml`:** Before moving out of the monorepo, move or remove the two relative `[[test]]` entries in `Cargo.toml` (`path = "../../tests/schedule_shuffle_fuzz.rs"` and `path = "../../tests/soak_10k_reloads.rs"`) into `tests/` inside this crate (or a workspace-level integration repo).
2. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,manifest,host_sys,macros}`, `bevy_*` (`0.20.1`), `toml`, `semver`, `tracing`, `libloading`, `proptest`, and `ssh-key` in `Cargo.toml`.
3. **Fixtures & Root Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`. Note `e2e`, `ffi`, `lifecycle`, and `scheduling` suites require guest fixture `.dylib`/`.so` builds pointed to by `BAM_FIXTURE_DIR`.
