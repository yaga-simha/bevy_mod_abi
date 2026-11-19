# `bevy_abi_mod_manifest`

Zero-Bevy (`#![forbid(unsafe_code)]`) `mod.toml` manifest parser, SemVer validator, and Kahn topological dependency DAG resolver for [`bevy_abi_mod`](../bevy_abi_mod).

## Responsibilities

- **`ModTomlManifest` Parser:** Parses `[mod]` metadata (`id`, `name`, `version`, `abi_version`, `language`, `entry`/`library`), `[dependencies]`, and `[optional_dependencies]` from `mod.toml` files without depending on Bevy.
- **ABI SemVer Compatibility:** Checks optional `[mod].abi_version` requirements against host [`ABI_VERSION`](../bevy_abi_mod_sys) using Cargo-compatible SemVer matching.
- **`ModDependencyGraph` DAG Resolver:** Validates required and optional SemVer dependency ranges across a directory of mods, rejects missing required dependencies and dependency cycles with structured [`ManifestError`] diagnostics, and computes deterministic topological load order via Kahn's algorithm.
