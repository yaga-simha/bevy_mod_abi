#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `guest_sdk`
//!
//! Ergonomic, Bevy-independent SDK for guest mod authors.
//! Wraps raw `abi_contract` C-ABI structs and function pointers into safe abstractions.

pub use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, ModAbiVersionFn, ModInitFn, ModShutdownFn, RawChunkIter,
    RawCommandQueue, RegisterSystemFn, StrView, SymbolEntry, SystemDescriptor, ABI_VERSION_MAJOR,
    ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
};

/// Macro to export a mod's lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`).
#[macro_export]
macro_rules! export_mod {
    ($init_fn:path, $shutdown_fn:path) => {
        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $init_fn(get_proc_address, host_api, host_context)
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $shutdown_fn(host_context)
        }
    };
}
