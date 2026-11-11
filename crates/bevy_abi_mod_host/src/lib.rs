#![forbid(unsafe_code)]

//! Safe idiomatic Bevy host crate for `bevy_abi_mod`.
//!
//! `bevy_abi_mod_host` provides Bevy ECS (`v0.20+`) application integration for loading,
//! executing, and hot-unloading native C-ABI mods compiled in Rust, C99, C++, or Zig.
//!
//! All `unsafe` FFI internals are isolated inside `bevy_abi_mod_host_sys`; this crate enforces
//! `#![forbid(unsafe_code)]`.

pub mod app_ext;
pub mod events;
pub mod plugin;
pub mod schedule_sets;

pub use bevy_abi_mod_macros::{AbiPod, ModExposable};
pub use bevy_abi_mod_sys;

pub use app_ext::ModAppExt;
pub use bevy_abi_mod_host_sys::{
    ComponentCollisionError, LoadedMod, ModErrorPolicy, ModErrorPolicyConfig, ModHost, ModId,
    ModLoadError, ModRegistry, is_abi_version_compatible,
};
pub use events::{
    LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
};
pub use plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
pub use schedule_sets::{ModNamedSystemSet, ModSchedule, ModSystemSet, StartupCompleted};

/// Convenient prelude module for Bevy games hosting native C-ABI mods.
pub mod prelude {
    pub use crate::app_ext::ModAppExt;
    pub use crate::events::{
        LoadedModInfo, ModLoaded, ModSystemError, ModSystemFaulted, ModUnloaded, SystemInitError,
    };
    pub use crate::plugin::{AbiModPlugin, ConfiguredAbiModPlugin};
    pub use crate::schedule_sets::{
        ModNamedSystemSet, ModSchedule, ModSystemSet, StartupCompleted,
    };
    pub use crate::{
        AbiPod, ComponentCollisionError, LoadedMod, ModErrorPolicy, ModExposable, ModHost, ModId,
        ModLoadError, ModRegistry,
    };
    pub use bevy_abi_mod_sys::{
        ComponentAccessDesc, ComponentAccessMode, ComponentUuid,
        ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, ExposableComponent,
        FfiEntity, FfiResult, FfiSlice, RunConditionCallback, ScheduleLabelId,
        ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor, StrView,
    };
}
