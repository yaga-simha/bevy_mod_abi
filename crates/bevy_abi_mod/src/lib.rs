#![forbid(unsafe_code)]

//! # `bevy_abi_mod` — Zero-Serialization Asymmetric C-ABI Native Modding for Bevy ECS
//!
//! Top-level host facade crate re-exporting [`bevy_abi_mod_host`], [`ModExposable`], [`AbiPod`],
//! and [`bevy_abi_mod_sys`]. All public host APIs in this crate enforce `#![forbid(unsafe_code)]`.

pub use bevy_abi_mod_host;
pub use bevy_abi_mod_host::*;
pub use bevy_abi_mod_macros;
