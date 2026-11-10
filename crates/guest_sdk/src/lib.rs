//! # `guest_sdk`
//!
//! Ergonomic, Bevy-independent SDK for guest mod authors.
//! Wraps raw `abi_contract` C-ABI structs and function pointers into safe abstractions.

pub use abi_contract::{
    ComponentAccessMode, ComponentAccessRequirement, ComponentUuid, EcsChunkIter, EntityId,
    GetProcAddressFn, HostApi, ModInitFn, ModShutdownFn, ModStatus, StrView,
    SystemAccessDescriptor, SystemRunCallback,
};

/// Macro to export a mod's lifecycle entry points.
#[macro_export]
macro_rules! export_mod {
    ($init_fn:path, $shutdown_fn:path) => {
        #[no_mangle]
        pub extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut core::ffi::c_void,
        ) -> $crate::ModStatus {
            $init_fn(get_proc_address, host_api, host_context)
        }

        #[no_mangle]
        pub extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut core::ffi::c_void,
        ) -> $crate::ModStatus {
            $shutdown_fn(host_context)
        }
    };
}
