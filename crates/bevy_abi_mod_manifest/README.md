# `bevy_abi_mod_manifest`

Zero-Bevy (`#![forbid(unsafe_code)]`) `mod.toml` manifest parser, path/symlink validator, SemVer checker, and Kahn topological dependency DAG resolver for [`bevy_mod_abi`](../bevy_mod_abi) (`v0.3.0`).

## Responsibilities

- **`ModTomlManifest` Parser & Path/Symlink Hardening:** Parses `[mod]` metadata (`id`, `name`, `version`, `abi_version`, `language`, `entry`/`library`), `[dependencies]`, and `[optional_dependencies]` from `mod.toml` files without depending on Bevy, enforcing safe mod identifiers (`validate_mod_id`), relative entry paths (`validate_manifest_entry_path`), and rejecting path traversal, root escapes, and symlinked mod binaries/paths.
- **ABI SemVer Compatibility:** Checks optional `[mod].abi_version` requirements against host [`ABI_VERSION`](../bevy_abi_mod_sys) (`0.3.0`) using Cargo-compatible SemVer matching.
- **`ModDependencyGraph` DAG Resolver:** Validates required and optional SemVer dependency ranges across a directory of mods, rejects missing required dependencies and dependency cycles with structured `ManifestError` diagnostics, and computes deterministic topological load order via Kahn's algorithm (shared by `bevy_abi_mod_host`, `bevy_abi_mod_bridges` `TopologicalOverlay`, `bevy_abi_mod_watcher`, and `cargo-bam`).
