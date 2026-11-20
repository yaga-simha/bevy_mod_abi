# Agent Guide: `bevy_abi_mod_manifest`

Crate guide for **`bevy_abi_mod_manifest`** (`v0.3.0`, ABI `0x0003_0000`) — zero-Bevy `mod.toml` parser, path/symlink validator, SemVer checker, and Kahn topological DAG resolver.

---

## 1. Crate Identity
- **Name:** `bevy_abi_mod_manifest` (`version = "0.3.0"`, `edition = "2024"`, MSRV `1.95`)
- **Role:** Shared non-Bevy manifest & dependency graph layer used by `bevy_abi_mod_host`, `bevy_abi_mod_bridges` (`TopologicalOverlay`), `bevy_abi_mod_watcher`, and `cargo-bam`.

---

## 2. Safety & Dependency Boundaries
- **Safety:** **`#![forbid(unsafe_code)]`** at crate root.
- **Zero Bevy Dependencies:** Allowed dependencies are `bevy_abi_mod_sys`, `semver`, `serde`, and `toml` ONLY (`tempfile` in `[dev-dependencies]`). **NEVER** add any `bevy*` dependency.

---

## 3. Owned Surface & Security Guards
- **`ModTomlManifest` (`src/lib.rs`):** Parses `[mod]` (`id`, `name`, `version`, `abi_version`, `language`, `entry`/`library`), `[dependencies]`, and `[optional_dependencies]`, enforcing bounded file reads (`MAX_MANIFEST_BYTES = 1 MiB`) and Cargo-compatible SemVer checking against `bevy_abi_mod_sys::ABI_VERSION_*` (`0.3.0`).
- **Path & Identifier Hardening:** `validate_mod_id` and `validate_manifest_entry_path` reject `..`/`.` traversal, absolute/backslash/UNC paths, Windows drive prefixes, NTFS ADS (`:`), NUL/control bytes, trailing dots/spaces, and Windows reserved names; `resolve_library_path` enforces per-segment `symlink_metadata` anti-symlink checks and canonical root confinement.
- **Topological DAG Sorter:** `ModDependencyGraph` resolves required and optional SemVer ranges via Kahn's algorithm, rejecting cycles and missing/incompatible dependencies with structured `ManifestError` variants.

---

## 4. Strict Engineering Rules
1. **Strict Path Confinement:** All manifest IDs, entry paths, and library resolutions MUST validate lexically via `validate_mod_id` / `validate_manifest_entry_path` AND reject symlink escapes outside the mod directory.
2. **Error Compatibility:** Preserve structured `ManifestError` variants consumed by `bevy_abi_mod_host::mod_dag` and `cargo-bam::dag`.
3. **Zero Panics on Untrusted `mod.toml`:** Reject malformed UTF-8/TOML, oversized files (`> MAX_MANIFEST_BYTES`), and invalid paths via `Result<_, ManifestError>` without `.unwrap()` or `.expect()`.

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
1. **Replace Workspace Dependencies:** In `Cargo.toml`, replace `{ workspace = true }` on `bevy_abi_mod_sys` (`0.3.0`), `semver`, `serde` (`features = ["derive"]`), `toml`, and `[dev-dependencies]` (`tempfile`) with explicit versions or git/crates.io coordinates.
2. **Update Repository URL & Root Configs:** Set `repository` in `Cargo.toml` if moved to its own Git repo; copy `.gitignore`, `.gitattributes`, `.typos.toml`, `clippy.toml`, `deny.toml`, `rust-toolchain.toml`, and `LICENSE-MIT` / `LICENSE-APACHE`.
3. **Downstream Consumers:** Verify any `ManifestError` or `ModTomlManifest` changes against `bevy_abi_mod_host` (`mod_dag.rs`), `bevy_abi_mod_watcher`, and `cargo-bam`.
