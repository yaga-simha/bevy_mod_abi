//! Entry-point export macros for guest `.cdylib` mods.

/// Exports a mod's C-ABI lifecycle entry points (`bevy_abi_mod_abi_version`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) with guest-side panic containment
/// and automatic [`GuestApp`](crate::GuestApp) state teardown.
///
/// Supports three forms:
/// 1. `export_mod!(setup_fn)` — takes `fn(&mut GuestApp<'_>) -> FfiResult`.
/// 2. `export_mod!(plugin: MyModType)` — takes a type implementing [`GuestPlugin`](crate::GuestPlugin).
/// 3. `export_mod!(init_fn, shutdown_fn)` — low-level C-ABI init/shutdown functions.
///
/// # Example
///
/// ```no_run
/// use bevy_abi_mod_guest::{export_mod, FfiResult, GuestApp};
///
/// fn setup(_app: &mut GuestApp<'_>) -> FfiResult {
///     FfiResult::OK
/// }
/// export_mod!(setup);
/// ```
#[macro_export]
macro_rules! export_mod {
    ($build_fn:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by host during mod loading with valid handshake pointers.
            unsafe {
                $crate::raw::run_lifecycle_init::<$crate::GuestApp<'static>, _>(
                    get_proc_address,
                    host_api,
                    host_context,
                    $build_fn,
                )
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| {
                $crate::cleanup_registered_guest_state();
                $crate::FfiResult::OK
            })
        }
    };
    (plugin: $plugin_ty:ty) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by host during mod loading with valid handshake pointers.
            unsafe {
                $crate::raw::run_lifecycle_init::<$crate::GuestApp<'static>, _>(
                    get_proc_address,
                    host_api,
                    host_context,
                    <$plugin_ty as $crate::GuestPlugin>::build,
                )
            }
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            _host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            let status =
                $crate::catch_guest_panic(|| <$plugin_ty as $crate::GuestPlugin>::shutdown());
            $crate::cleanup_registered_guest_state();
            status
        }
    };
    ($init_fn:path, $shutdown_fn:path) => {
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_abi_version() -> u32 {
            $crate::ABI_VERSION_PACKED
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            get_proc_address: $crate::GetProcAddressFn,
            host_api: *const $crate::HostApi,
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            $crate::catch_guest_panic(|| $init_fn(get_proc_address, host_api, host_context))
        }

        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(
            host_context: *mut ::core::ffi::c_void,
        ) -> $crate::FfiResult {
            let status = $crate::catch_guest_panic(|| $shutdown_fn(host_context));
            $crate::cleanup_registered_guest_state();
            status
        }
    };
}
