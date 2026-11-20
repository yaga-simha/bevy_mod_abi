#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![deprecated(note = "Use `bevy_mod_abi` instead")]

//! Deprecated compatibility shim re-exporting [`bevy_mod_abi`].
//!
//! `bevy_mod_abi` is the canonical top-level Bevy host facade crate for `ABI 0.3.0`.
//! Depend on `bevy_mod_abi` directly in `Cargo.toml` and import from `bevy_mod_abi::*` or
//! `bevy_mod_abi::prelude::*`.

pub use bevy_mod_abi;
pub use bevy_mod_abi::*;

/// Deprecated compatibility prelude forwarding [`bevy_mod_abi::prelude`].
pub mod prelude {
    pub use bevy_mod_abi::prelude::*;
}
