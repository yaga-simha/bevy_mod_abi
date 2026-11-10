#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `guest_sdk`
//!
//! Ergonomic, Bevy-independent SDK for guest mod authors.
//! Wraps raw `abi_contract` C-ABI structs and function pointers into safe abstractions,
//! and catches unwinding panics inside the guest's own `std` runtime before crossing
//! the `extern "C"` boundary.

pub use abi_contract::{
    ComponentAccessDesc, ComponentAccessMode, ComponentUuid, DynamicSystemCallback,
    ExposableComponent, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi,
    IterColumnMutableFn, IterColumnReadonlyFn, IterColumnStrideFn, IterEntitiesFn,
    IterMarkChangedFn, IterNextChunkFn, ModAbiVersionFn, ModInitFn, ModShutdownFn, RawChunkIter,
    RawCommandQueue, RegisterSystemFn, StrView, SymbolEntry, SystemDescriptor, ABI_VERSION_MAJOR,
    ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
};

/// Execute a closure inside `std::panic::catch_unwind` within the guest's own Rust runtime,
/// converting any unwinding panic into `FfiResult::PANIC_CAUGHT` before crossing the `extern "C"`
/// boundary.
///
/// Why this is done on the guest side:
/// Each `.cdylib` links its own isolated `std` runtime. Unwinding a guest panic into the host's
/// `std::panic::catch_unwind` is classified by the Rust Reference as catching a foreign exception
/// (which has unspecified behavior and may abort the process). Catching the unwind inside the
/// guest's own `std` runtime and returning `FfiResult::PANIC_CAUGHT` by value is 100% well-defined.
#[inline]
pub fn catch_guest_panic<F>(f: F) -> FfiResult
where
    F: FnOnce() -> FfiResult,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(status) => status,
        Err(_) => FfiResult::PANIC_CAUGHT,
    }
}

/// Macro to export a mod's lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) with guest-side panic containment.
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
            $crate::catch_guest_panic(|| $init_fn(get_proc_address, host_api, host_context))
        }

        #[no_mangle]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| $shutdown_fn(host_context))
        }
    };
}

/// Macro to wrap a guest Rust system function into a C-ABI `DynamicSystemCallback`
/// (`unsafe extern "C" fn`) that catches guest panics and returns `FfiResult::PANIC_CAUGHT`.
#[macro_export]
macro_rules! wrap_system_callback {
    ($vis:vis fn $wrapper_name:ident calls $inner_fn:path) => {
        $vis unsafe extern "C" fn $wrapper_name(
            iter: *mut $crate::RawChunkIter,
            commands: *mut $crate::RawCommandQueue,
            user_data: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                // SAFETY: Forwarding opaque handles received from the host executor.
                unsafe { $inner_fn(iter, commands, user_data) }
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::ffi::c_void;

    unsafe fn panicking_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        panic!("mod crash");
    }

    unsafe fn healthy_system(
        _iter: *mut RawChunkIter,
        _commands: *mut RawCommandQueue,
        _user_data: *mut c_void,
    ) -> FfiResult {
        FfiResult::OK
    }

    wrap_system_callback!(fn c_panicking_system calls panicking_system);
    wrap_system_callback!(fn c_healthy_system calls healthy_system);

    #[test]
    fn test_guest_panic_containment_returns_panic_caught() {
        // SAFETY: Passing null opaque pointers to test callbacks that do not dereference them.
        let status = unsafe {
            c_panicking_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(status, FfiResult::PANIC_CAUGHT);

        // SAFETY: Passing null opaque pointers to healthy test callback.
        let ok_status = unsafe {
            c_healthy_system(
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null_mut(),
            )
        };
        assert_eq!(ok_status, FfiResult::OK);
    }
}
