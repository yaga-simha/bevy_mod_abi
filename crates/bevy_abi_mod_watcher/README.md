# `bevy_abi_mod_watcher`

Safe (`#![forbid(unsafe_code)]`) debounced filesystem hot-reload watcher for [`bevy_abi_mod`](../bevy_abi_mod) (`ABI 0.3.0`, `v0.3.0`).

## Overview

- **`ModHotReloadPlugin` / `ModHotReloadWatcher`:** Recursively monitors a mods directory for native shared libraries (`.so`, `.dylib`, `.dll`) and `mod.toml` manifests via `notify-debouncer-full` (pairing seamlessly with `cargo bam watch --out-dir`), validates SemVer dependency DAG constraints, and executes cascading reverse/forward topological hot-reloads and unloads in Bevy `PreUpdate`.
- **`ModHotReloadHistory` / `ModHotReloadOutcome` / `ModHotReloadAction`:** Records chronological load, reload, unload, and validation failure outcomes as a Bevy resource.
