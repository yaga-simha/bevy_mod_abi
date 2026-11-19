#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) dynamic `bevy_reflect`, Bevy Remote Protocol (`bevy_remote`
//! BRP), and `bevy_settings` TOML bridges for `bevy_abi_mod` (`ABI 0.3.0`).
//!
//! Exposes both witness-backed and declarative ABI `0.3` `BamFieldDesc` components and singleton
//! resources to Bevy's reflection registry (`AppTypeRegistry` / `ReflectFromPtr`), JSON-RPC
//! remote inspection/mutation (`bevy_abi_mod/*` and standard `world.*` BRP methods), and
//! persistent TOML settings files.

pub mod reflect;

pub use bevy_abi_mod_host_sys::{
    reflect_entity_component, reflect_entity_component_mut, reflect_resource_component,
    reflect_resource_component_mut,
};
pub use reflect::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry, ModReflectRemoteExt,
    ModReflectWorldExt, ModSettingsError, ModSettingsRegistry, TypedReflectBridgeEntry,
    WitnessReflectSchema, handle_mod_brp_request, install_mod_brp_methods,
};

/// Convenient prelude re-exporting reflection, BRP, and settings plugins, traits, and error types.
pub mod prelude {
    pub use crate::reflect::{
        ModReflectAppExt, ModReflectError, ModReflectPlugin, ModReflectRegistry,
        ModReflectRemoteExt, ModReflectWorldExt, ModSettingsError, ModSettingsRegistry,
    };
}
