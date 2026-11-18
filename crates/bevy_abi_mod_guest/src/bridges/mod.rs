//! Ergonomic `#![forbid(unsafe_code)]`, zero-Bevy guest wrappers for bridged Bevy standard
//! components, resources, isolated mod assets, and state machines.

pub mod asset;
pub mod bsn;
pub mod reflect;
pub mod state;
pub mod std_types;

use bevy_abi_mod_guest_sys::Host;
use bevy_abi_mod_sys::FfiResult;

use crate::app::{GuestApp, SystemContext};

pub use asset::{GuestAssetHandle, GuestAssetHostExt, GuestAssetServer, TypedAssetHandle};
pub use bsn::{GuestCommandsBsnExt, GuestScene, resolve_scene_patch_type};
pub use reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    resolve_reflected_handle, resolve_settings_handle,
};
pub use state::{
    GuestCommandsStateExt, GuestStateBridge, GuestStateHandles, GuestStateQueryBuilderExt,
    GuestStateSystemBuilderExt, on_enter_schedule, on_exit_schedule, on_transition_schedule,
};
pub use std_types::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, GuestNameBridge, GuestStdCommandsExt,
    GuestStdHandles, GuestStdQueryBuilderExt,
};
#[cfg(feature = "derive")]
pub use std_types::{
    GuestGlobalTransformBridge, GuestKeyboardInputBridge, GuestMouseInputBridge, GuestStdTypes,
    GuestTimeFixedBridge, GuestTimeRealBridge, GuestTimeVirtualBridge, GuestTransformBridge,
    GuestVisibilityBridge, GuestWireBridge,
};

/// Ergonomic cross-bridge resolution helpers on [`Host`], [`GuestApp`], and [`SystemContext`].
pub trait GuestBridgesHostExt {
    /// Resolves the primary host-exposed state bridge (`slot = 0`).
    fn state_bridge(&self) -> Result<GuestStateBridge, FfiResult>;
    /// Resolves a host-exposed state bridge at `slot`.
    fn state_bridge_slot(&self, slot: usize) -> Result<GuestStateBridge, FfiResult>;
    /// Resolves the `Name` string bridge.
    fn std_name_bridge(&self) -> Result<GuestNameBridge, FfiResult>;
    /// Resolves all standard Bevy wire/string bridges (`Transform`, `GlobalTransform`,
    /// `Visibility`, `Name`, `Time<*>`, `ButtonInput<*>`).
    #[cfg(feature = "derive")]
    fn std_bridges(&self) -> Result<GuestStdTypes, FfiResult>;
}

impl GuestBridgesHostExt for Host {
    #[inline]
    fn state_bridge(&self) -> Result<GuestStateBridge, FfiResult> {
        GuestStateBridge::resolve_primary(self)
    }

    #[inline]
    fn state_bridge_slot(&self, slot: usize) -> Result<GuestStateBridge, FfiResult> {
        GuestStateBridge::resolve_slot(self, slot)
    }

    #[inline]
    fn std_name_bridge(&self) -> Result<GuestNameBridge, FfiResult> {
        GuestNameBridge::resolve(self)
    }

    #[cfg(feature = "derive")]
    #[inline]
    fn std_bridges(&self) -> Result<GuestStdTypes, FfiResult> {
        GuestStdTypes::resolve_all(self)
    }
}

impl GuestBridgesHostExt for GuestApp<'_> {
    #[inline]
    fn state_bridge(&self) -> Result<GuestStateBridge, FfiResult> {
        self.host().state_bridge()
    }

    #[inline]
    fn state_bridge_slot(&self, slot: usize) -> Result<GuestStateBridge, FfiResult> {
        self.host().state_bridge_slot(slot)
    }

    #[inline]
    fn std_name_bridge(&self) -> Result<GuestNameBridge, FfiResult> {
        self.host().std_name_bridge()
    }

    #[cfg(feature = "derive")]
    #[inline]
    fn std_bridges(&self) -> Result<GuestStdTypes, FfiResult> {
        self.host().std_bridges()
    }
}

impl GuestBridgesHostExt for SystemContext<'_> {
    #[inline]
    fn state_bridge(&self) -> Result<GuestStateBridge, FfiResult> {
        self.host().state_bridge()
    }

    #[inline]
    fn state_bridge_slot(&self, slot: usize) -> Result<GuestStateBridge, FfiResult> {
        self.host().state_bridge_slot(slot)
    }

    #[inline]
    fn std_name_bridge(&self) -> Result<GuestNameBridge, FfiResult> {
        self.host().std_name_bridge()
    }

    #[cfg(feature = "derive")]
    #[inline]
    fn std_bridges(&self) -> Result<GuestStdTypes, FfiResult> {
        self.host().std_bridges()
    }
}
