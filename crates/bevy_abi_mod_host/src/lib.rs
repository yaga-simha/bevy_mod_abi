#![forbid(unsafe_code)]

//! # Safe Idiomatic Host Crate for `bevy_abi_mod`
//!
//! `bevy_abi_mod_host` provides safe, idiomatic Bevy ECS (`v0.20+`) application integration
//! for loading, executing, and hot-unloading native C-ABI mods compiled in Rust, C99, C++, or Zig.
//!
//! All `unsafe` FFI internals are isolated inside `bevy_abi_mod_host_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`.

pub mod plugin;

#[doc(hidden)]
pub use bevy_abi_mod_host_sys as sys;

pub use bevy_abi_mod_macros::{AbiPod, ModExposable};
pub use bevy_abi_mod_sys;

pub use bevy_abi_mod_host_sys::{
    ComponentCollisionError, LoadedMod, ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModId,
    ModLoadError, ModRegistry, ModSchedule, ModSystemError, ModSystemFaulted, ModSystemSet,
    is_abi_version_compatible,
};
pub use plugin::{
    AbiModPlugin, ConfiguredAbiModPlugin, LoadedModInfo, ModAppExt, ModLoaded, ModNamedSystemSet,
    ModUnloaded,
};

/// Convenient prelude module for Bevy games hosting native C-ABI mods.
pub mod prelude {
    pub use crate::plugin::{
        AbiModPlugin, ConfiguredAbiModPlugin, LoadedModInfo, ModAppExt, ModLoaded,
        ModNamedSystemSet, ModUnloaded,
    };
    pub use crate::{
        AbiPod, ComponentCollisionError, LoadedMod, ModErrorPolicy, ModExposable, ModHost, ModId,
        ModLoadError, ModRegistry, ModSchedule, ModSystemError, ModSystemFaulted, ModSystemSet,
    };
    pub use bevy_abi_mod_sys::{
        ComponentAccessDesc, ComponentAccessMode, ComponentUuid,
        ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, ExposableComponent,
        FfiEntity, FfiResult, FfiSlice, RunConditionCallback, ScheduleLabelId,
        ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor, StrView,
    };
}
