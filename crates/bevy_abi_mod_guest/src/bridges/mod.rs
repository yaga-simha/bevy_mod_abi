//! Ergonomic `#![forbid(unsafe_code)]`, zero-Bevy guest wrappers for bridged Bevy standard
//! components, resources, isolated mod assets, and state machines.

pub mod asset;
pub mod state;
pub mod std_types;

pub use asset::{GuestAssetHandle, GuestAssetHostExt, GuestAssetServer, TypedAssetHandle};
pub use state::{
    GuestCommandsStateExt, GuestStateBridge, GuestStateSystemBuilderExt, on_enter_schedule,
    on_exit_schedule, on_transition_schedule,
};
pub use std_types::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, GuestNameBridge,
};
#[cfg(feature = "derive")]
pub use std_types::{
    GuestGlobalTransformBridge, GuestKeyboardInputBridge, GuestMouseInputBridge,
    GuestTimeFixedBridge, GuestTimeRealBridge, GuestTimeVirtualBridge, GuestTransformBridge,
    GuestVisibilityBridge, GuestWireBridge,
};
