//! Safe (`#![forbid(unsafe_code)]`), zero-Bevy guest helpers for dynamic reflection, BRP, and
//! settings bridges.

use bevy_abi_mod_guest_sys::{Host, resolve_component};
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiResult};
use core::ffi::CStr;
use std::ffi::CString;

/// BRP method path listing mod-registered components, resources, and witness schemas.
pub const BRP_METHOD_LIST_COMPONENTS: &str = "bevy_abi_mod/list_components";
/// BRP method path inspecting mod-registered dynamic or reflected components on an entity.
pub const BRP_METHOD_GET_COMPONENTS: &str = "bevy_abi_mod/get_components";
/// BRP method path mutating mod-registered dynamic or reflected components on an entity.
pub const BRP_METHOD_MUTATE_COMPONENTS: &str = "bevy_abi_mod/mutate_components";
/// BRP method path inserting or updating a witness-backed or reflected component on an entity.
pub const BRP_METHOD_INSERT_COMPONENTS: &str = "bevy_abi_mod/insert_components";
/// BRP method path inspecting a mod-registered singleton resource.
pub const BRP_METHOD_GET_RESOURCES: &str = "bevy_abi_mod/get_resources";
/// BRP method path mutating a mod-registered singleton resource.
pub const BRP_METHOD_MUTATE_RESOURCES: &str = "bevy_abi_mod/mutate_resources";

/// Resolves a host-registered reflected or witness-backed component/resource handle by name.
pub fn resolve_reflected_handle(
    host: &Host,
    canonical_name: &str,
) -> Result<BamComponent, FfiResult> {
    let c_name = CString::new(canonical_name).map_err(|_| FfiResult::INVALID_ARGUMENT)?;
    resolve_component(
        host,
        Some(c_name.as_c_str()),
        Some(ComponentUuid::from_canonical_name(canonical_name)),
    )
    .or_else(|_| resolve_component(host, Some(c_name.as_c_str()), None))
}

/// Resolves a host-exposed `SettingsGroup` resource handle by canonical name or short alias.
#[inline]
pub fn resolve_settings_handle(host: &Host, name: &CStr) -> Result<BamComponent, FfiResult> {
    resolve_component(host, Some(name), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brp_method_constants_use_bevy_abi_mod_namespace() {
        for method in [
            BRP_METHOD_LIST_COMPONENTS,
            BRP_METHOD_GET_COMPONENTS,
            BRP_METHOD_MUTATE_COMPONENTS,
            BRP_METHOD_INSERT_COMPONENTS,
            BRP_METHOD_GET_RESOURCES,
            BRP_METHOD_MUTATE_RESOURCES,
        ] {
            assert!(method.starts_with("bevy_abi_mod/"));
        }
    }
}
