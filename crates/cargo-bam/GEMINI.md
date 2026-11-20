# Agent Guide: `cargo-bam`

Crate guide for **`cargo-bam`** (`v0.3.0`, ABI `0x0003_0000`) — standalone, `#![forbid(unsafe_code)]`, zero-Bevy CLI toolchain for authoring, building, signing, packaging, and diagnosing mods.

---

## 1. Crate Identity
- **Name:** `cargo-bam` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Provides the `cargo bam` / `cargo-bam` binary and `cargo_bam` library covering project scaffolding, polyglot SDK export, shared-library builds, `.bampack` packaging, live watch rebuilds, DAG checking, diagnostics, and Ed25519 keygen/signing/verification.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root. Zero `unsafe` code.
- **Zero Bevy Dependencies:** Depends strictly on `bevy_abi_mod_{sys,manifest,sdk_gen}` + `bevy_abi_mod_trust` (`features = ["keygen"]`), `clap`, `semver`, `serde`, `sha2`, `thiserror`, `toml` (plus `tempfile` in `[dev-dependencies]`). **NEVER** add any `bevy*` dependency.

---

## 3. Owned Surface & CLI Subcommands
- **Scaffolding (`scaffold.rs`):** `new --lang <rust|c99|cpp17|zig>` — Rust scaffold generates `0.3.0` `bevy_abi_mod_guest` (`features = ["derive"]`) with zero-boilerplate `#[derive(AbiPod, GuestComponent)]` + `.query_typed::<&mut T>()` / `ctx.single_query::<&mut T>()` (zero compile-time `CARGO_MANIFEST_DIR` path leaks); C99/C++17/Zig scaffolds use bounded `bam_host_api_copy_bounded` helpers without unsafe `*api` derefs.
- **SDK Export (`sdk.rs`):** `export-sdk` (alias `header`) emitting `bevy_abi_mod.h`, `bevy_abi_mod.hpp`, and `bevy_abi_mod.zig`.
- **Build, Watch & Package (`builder.rs`, `watch.rs`, `pack.rs`, `dag.rs`):** `build`, `watch`, `pack`, and `check-mods` enforcing `validate_mod_id` / `validate_manifest_entry_path`, `validated_stage_dir`, atomic `.sig`-before-dylib staging, bounded file reads, and cert-backed `.bampack` verification (asset depth/size/case-collision guards).
- **Doctor & Trust (`doctor.rs`, `signing.rs`):** `doctor`, `keygen`, `sign`, and `verify` (including `--krl` revocation list checking).

---

## 4. Strict Engineering Rules
1. **Path Security & No Build-Host Path Leaks:** Reject traversal/symlink escapes in staging, packing, and manifests; never embed compile-time developer paths (`env!("CARGO_MANIFEST_DIR")`) into generated `new` project templates.
2. **Zero Panics in CLI/Lib Paths:** Maintain zero `.unwrap()` / `.expect()` calls in `src/`; surface errors cleanly via `thiserror`.
3. **Keep Scaffolds Tested End-to-End:** Ensure integration tests (`tests/cli_integration.rs`, `tests/pack_watch_doctor_e2e.rs`) validate and compile the unmodified generated scaffolds.

---

## 5. Standalone Verification Commands

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-features
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --all-features
cargo tree | sed 's/ (.*//' | grep -v bevy_abi_mod | grep -i bevy   # MUST print NOTHING
typos
```

---

## 6. Standalone Repository Split Checklist
1. **Replace Workspace Dependencies:** Replace `{ workspace = true }` on `bevy_abi_mod_{sys,manifest,sdk_gen,trust}` (`"0.3.0"`), `clap`, `semver`, `serde`, `sha2`, `thiserror`, `toml`, and `[dev-dependencies]` (`tempfile`) in `Cargo.toml`.
2. **Update Scaffold Integration Tests After Split:** In `tests/cli_integration.rs` and `tests/pack_watch_doctor_e2e.rs`, `--guest-sdk-path` currently resolves `CARGO_MANIFEST_DIR/../bevy_abi_mod_guest`; update those test paths or point them to published/checked-out `bevy_abi_mod_guest` when `cargo-bam` lives in its own repository.
3. **Root Tooling Configs:** Copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
