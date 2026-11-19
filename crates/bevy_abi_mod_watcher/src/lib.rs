#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) debounced filesystem hot-reload watcher for `bevy_abi_mod`
//! (`ABI 0.3.0`).
//!
//! Monitors a mods directory recursively for native shared libraries (`.so`, `.dylib`, `.dll`)
//! and declarative `mod.toml` manifests via `notify-debouncer-full`, validates SemVer DAG
//! constraints, and drives cascading topological reloads and unloads in Bevy `PreUpdate`.

pub mod watcher;

pub use watcher::{
    ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
    ModHotReloadWatcher,
};

/// Convenient prelude re-exporting [`ModHotReloadPlugin`], [`ModHotReloadWatcher`], and outcome types.
pub mod prelude {
    pub use crate::watcher::{
        ModHotReloadAction, ModHotReloadHistory, ModHotReloadOutcome, ModHotReloadPlugin,
        ModHotReloadWatcher,
    };
}
