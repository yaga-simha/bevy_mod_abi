//! Safe (`#![forbid(unsafe_code)]`) host bridges for Bevy standard components, resources,
//! isolated mod assets, and Bevy states.

#[cfg(feature = "bridge-asset")]
pub mod asset;
#[cfg(feature = "bridge-state")]
pub mod state;
#[cfg(feature = "bridge-std-types")]
pub mod std_types;

#[cfg(feature = "bridge-asset")]
pub use asset::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
#[cfg(feature = "bridge-state")]
pub use state::{ModNextState, ModStateError, ModStateExt, ModStateSpec};
#[cfg(feature = "bridge-std-types")]
pub use std_types::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, StdTypesBridgePlugin,
    register_std_types_bridges,
};
