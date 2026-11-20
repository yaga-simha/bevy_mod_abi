# Agent Guide: `bevy_abi_mod_guest`

Crate guide for **`bevy_abi_mod_guest`** (`v0.3.0`, ABI `0x0003_0000`) — safe (`#![forbid(unsafe_code)]`), zero-Bevy Rust guest SDK for native `.cdylib` mods.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_guest` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.87`)
- **Role:** Safe, idiomatic guest SDK for authoring decoupled native `.cdylib` mods with zero Bevy dependencies and mismatched-toolchain ABI isolation.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. All FFI calls and panic guards live in `bevy_abi_mod_guest_sys`.
- **Zero Bevy Dependencies:** Depends strictly on `bevy_abi_mod_sys`, `bevy_abi_mod_guest_sys`, and optional `bevy_abi_mod_macros` (`feature = "derive"`, `default = []`). **NEVER** add any `bevy*` dependency.

---

## 3. Owned Surface & Feature Flags
- **Features:** `default = []`, `derive = ["dep:bevy_abi_mod_macros"]` (re-exporting `#[derive(AbiPod, GuestComponent, ModComponent)]` in `lib.rs` and `prelude`).
- **Guest Entry & Lifecycle (`lib.rs`, `lifecycle.rs`, `component.rs`):** `GuestPlugin`, `GuestApp`, `export_mod!` (per-World `GuestInstance` isolation + optional `save_state`/`restore_state`), `GuestComponent` (zero-boilerplate derive via `#[derive(AbiPod, GuestComponent)] #[guest_component(name = "...", summary_tick, ...)]` or `#[mod_component(..., guest)]`).
- **Typed Queries & Scheduling (`app/`, `iter/`):** `SystemBuilder` (`.query_typed::<Q>()` supporting 1..=16 `QueryTuple` terms + `Or<(T0..T7)>`, `Allow<T>`, `Has<T>`, `RemovedComponents`, `GuestRes<R>`, `GuestResMut<R>`, schedules, `.in_set`/`.before`/`.after`/`.after_weak`/`.main_thread`, chainable AND-composed `.run_if`/`.run_if_fn`), `SystemContext::single_query`, `TypedChunkIter::{for_each, par_for_each, par_for_each_chunk_readonly, up_cascade, count}`, `ScheduleSetBuilder`, `RelationDescBuilder`, and `ObserverBuilder` (chainable AND-composed `.run_if` / `.run_if_fn`).
- **Commands & Bridges (`commands.rs`, `commands_lifecycle.rs`, `bridges/`, `raw.rs`):** `GuestCommands` (`spawn`, `spawn_persistent`, `spawn_batch`, `FfiChildOf`, `despawn_all`, `trigger`, `GuestTransaction` via `DerefMut<Target = GuestCommands>`), `GuestComponentOptions`, `std_types`, `GuestAssetServer`, `GuestStateBridge`, `GuestScene`, `reflect`, and `guest_log!`.

---

## 4. Strict Engineering Rules
1. **Zero Boilerplate + Backwards Compatibility:** Keep `#[derive(AbiPod, GuestComponent)]` and `query_typed` / `single_query` ergonomic while preserving compatibility with existing manual `impl GuestComponent` structs.
2. **Per-World Isolation & Allocator Ownership:** Never store mod state in global `static`s (`GuestPlugin` state belongs inside `GuestInstance`); never drop host-owned memory or pass Rust `Vec`/`String`/`Box` across FFI.
3. **Zero Non-Test Panics:** Maintain zero `.unwrap()` / runtime `panic!` calls in `src/`; fail closed via `FfiResult`.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
cargo test --no-default-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
MIRIFLAGS="-Zmiri-tree-borrows -Zmiri-strict-provenance" rustup run nightly cargo miri test --all-features
cargo tree | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,guest_sys,macros}` (`"0.3.0"`) in `Cargo.toml`.
2. **Preserve MSRV `1.87` & `default = []`:** Keep `rust-version = "1.87"` and keep `derive` optional (`default = []`) so minimal/macro-free guests stay dependency-light.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
