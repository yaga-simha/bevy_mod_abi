//! Entry-point export macros (`export_mod!`) for guest `.cdylib` mods.

/// Exports a guest mod's mandatory ABI `0.2` C entry points (`bevy_abi_mod_manifest`,
/// `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown`) backed by per-World
/// [`GuestInstance`](crate::raw::GuestInstance) heap state and panic containment, with zero
/// process-global mutable state.
///
/// # Supported Forms
///
/// 1. **Idiomatic `GuestPlugin` (`export_mod!(plugin: MyMod)`)**:
///    Uses `<MyMod as GuestPlugin>::MANIFEST`, instantiates `MyMod::default()` per `World`, runs
///    `MyMod::build(&mut app)`, and runs `MyMod::on_shutdown(&mut self)` on unload.
/// 2. **Explicit manifest + setup function (`export_mod!(name: c"my_mod", version: c"0.1.0", setup_fn)`)**:
///    Uses the supplied `CStr` name/version literals and invokes `setup_fn(&mut GuestApp<'_>) -> FfiResult`.
/// 3. **Cargo package metadata + setup function (`export_mod!(setup_fn)`)**:
///    Derives `BamManifest::name` and `BamManifest::version` from `CARGO_PKG_NAME` and
///    `CARGO_PKG_VERSION` at compile time and invokes `setup_fn(&mut GuestApp<'_>) -> FfiResult`.
/// 4. **Raw C-ABI hooks (`export_mod!(manifest: MANIFEST, init: raw_init, shutdown: raw_shutdown)`)**:
///    Wraps low-level C-ABI functions in `catch_guest_result` / `catch_guest_unit`.
///
/// # Example
///
/// ```no_run
/// use bevy_abi_mod_guest::prelude::*;
///
/// #[derive(Default)]
/// pub struct MyMod;
///
/// impl GuestPlugin for MyMod {
///     const NAME: &'static core::ffi::CStr = c"my_mod";
///     const VERSION: &'static core::ffi::CStr = c"0.2.0";
///
///     fn build(&mut self, _app: &mut GuestApp<'_>) -> FfiResult {
///         FfiResult::OK
///     }
/// }
///
/// export_mod!(plugin: MyMod);
/// ```
#[macro_export]
macro_rules! export_mod {
    (plugin: $plugin_ty:ty) => {
        static __BAM_EXPORTED_MANIFEST: $crate::BamManifest =
            <$plugin_ty as $crate::GuestPlugin>::MANIFEST;

        /// Returns a pointer to the static [`BamManifest`]($crate::BamManifest) for this guest mod.
        ///
        /// # Safety
        /// Side-effect free; returns a pointer valid until library unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_manifest() -> *const $crate::BamManifest {
            &raw const __BAM_EXPORTED_MANIFEST
        }

        /// Initializes a per-World [`BamInstance`]($crate::raw::BamInstance) for this guest mod.
        ///
        /// # Safety
        /// Caller (host loader) must supply valid ABI `0.2` `api`, `load_ctx`, and `instance_out` pointers.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            api: *const $crate::raw::BamHostApi,
            load_ctx: *mut $crate::raw::BamLoadCtx,
            instance_out: *mut *mut $crate::raw::BamInstance,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by the host loader with valid ABI v0.2 handshake pointers.
            unsafe {
                $crate::raw::init_guest_instance(
                    api,
                    load_ctx,
                    instance_out,
                    &__BAM_EXPORTED_MANIFEST,
                    $crate::build_guest_plugin::<$plugin_ty>,
                )
            }
        }

        /// Tears down and frees a per-World [`BamInstance`]($crate::raw::BamInstance).
        ///
        /// # Safety
        /// `instance` must be either null or a pointer returned by `bevy_abi_mod_init` not yet freed.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(instance: *mut $crate::raw::BamInstance) {
            // SAFETY: Invoked once by the host with the `BamInstance*` returned by `bevy_abi_mod_init`.
            unsafe {
                $crate::raw::shutdown_guest_instance(instance);
            }
        }
    };
    (name: $name:expr, version: $version:expr, $build_fn:path $(,)?) => {
        static __BAM_EXPORTED_MANIFEST: $crate::BamManifest = $crate::BamManifest::new(
            ::core::ffi::CStr::as_ptr($name),
            ::core::ffi::CStr::as_ptr($version),
        )
        .with_state_callbacks(
            $crate::raw::guest_save_state_trampoline,
            $crate::raw::guest_restore_state_trampoline,
        );

        /// Returns a pointer to the static [`BamManifest`]($crate::BamManifest) for this guest mod.
        ///
        /// # Safety
        /// Side-effect free; returns a pointer valid until library unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_manifest() -> *const $crate::BamManifest {
            &raw const __BAM_EXPORTED_MANIFEST
        }

        /// Initializes a per-World [`BamInstance`]($crate::raw::BamInstance) for this guest mod.
        ///
        /// # Safety
        /// Caller (host loader) must supply valid ABI `0.2` `api`, `load_ctx`, and `instance_out` pointers.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            api: *const $crate::raw::BamHostApi,
            load_ctx: *mut $crate::raw::BamLoadCtx,
            instance_out: *mut *mut $crate::raw::BamInstance,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by the host loader with valid ABI v0.2 handshake pointers.
            unsafe {
                $crate::raw::init_guest_instance(
                    api,
                    load_ctx,
                    instance_out,
                    &__BAM_EXPORTED_MANIFEST,
                    |inst, load| $crate::build_guest_fn(inst, load, $build_fn),
                )
            }
        }

        /// Tears down and frees a per-World [`BamInstance`]($crate::raw::BamInstance).
        ///
        /// # Safety
        /// `instance` must be either null or a pointer returned by `bevy_abi_mod_init` not yet freed.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(instance: *mut $crate::raw::BamInstance) {
            // SAFETY: Invoked once by the host with the `BamInstance*` returned by `bevy_abi_mod_init`.
            unsafe {
                $crate::raw::shutdown_guest_instance(instance);
            }
        }
    };
    ($build_fn:path) => {
        static __BAM_EXPORTED_MANIFEST: $crate::BamManifest = $crate::BamManifest::new(
            concat!(env!("CARGO_PKG_NAME"), "\0")
                .as_ptr()
                .cast::<::core::ffi::c_char>(),
            concat!(env!("CARGO_PKG_VERSION"), "\0")
                .as_ptr()
                .cast::<::core::ffi::c_char>(),
        )
        .with_state_callbacks(
            $crate::raw::guest_save_state_trampoline,
            $crate::raw::guest_restore_state_trampoline,
        );

        /// Returns a pointer to the static [`BamManifest`]($crate::BamManifest) for this guest mod.
        ///
        /// # Safety
        /// Side-effect free; returns a pointer valid until library unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_manifest() -> *const $crate::BamManifest {
            &raw const __BAM_EXPORTED_MANIFEST
        }

        /// Initializes a per-World [`BamInstance`]($crate::raw::BamInstance) for this guest mod.
        ///
        /// # Safety
        /// Caller (host loader) must supply valid ABI `0.2` `api`, `load_ctx`, and `instance_out` pointers.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            api: *const $crate::raw::BamHostApi,
            load_ctx: *mut $crate::raw::BamLoadCtx,
            instance_out: *mut *mut $crate::raw::BamInstance,
        ) -> $crate::FfiResult {
            // SAFETY: Entry point invoked by the host loader with valid ABI v0.2 handshake pointers.
            unsafe {
                $crate::raw::init_guest_instance(
                    api,
                    load_ctx,
                    instance_out,
                    &__BAM_EXPORTED_MANIFEST,
                    |inst, load| $crate::build_guest_fn(inst, load, $build_fn),
                )
            }
        }

        /// Tears down and frees a per-World [`BamInstance`]($crate::raw::BamInstance).
        ///
        /// # Safety
        /// `instance` must be either null or a pointer returned by `bevy_abi_mod_init` not yet freed.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(instance: *mut $crate::raw::BamInstance) {
            // SAFETY: Invoked once by the host with the `BamInstance*` returned by `bevy_abi_mod_init`.
            unsafe {
                $crate::raw::shutdown_guest_instance(instance);
            }
        }
    };
    (manifest: $manifest:expr, init: $init_fn:path, shutdown: $shutdown_fn:path $(,)?) => {
        static __BAM_EXPORTED_MANIFEST: $crate::BamManifest = $manifest;

        /// Returns a pointer to the static [`BamManifest`]($crate::BamManifest) for this guest mod.
        ///
        /// # Safety
        /// Side-effect free; returns a pointer valid until library unload.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_manifest() -> *const $crate::BamManifest {
            &raw const __BAM_EXPORTED_MANIFEST
        }

        /// Initializes a per-World [`BamInstance`]($crate::raw::BamInstance) for this guest mod.
        ///
        /// # Safety
        /// Caller (host loader) must supply valid ABI `0.2` `api`, `load_ctx`, and `instance_out` pointers.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_init(
            api: *const $crate::raw::BamHostApi,
            load_ctx: *mut $crate::raw::BamLoadCtx,
            instance_out: *mut *mut $crate::raw::BamInstance,
        ) -> $crate::FfiResult {
            $crate::raw::catch_guest_result(|| {
                // SAFETY: Forwarding raw C-ABI handshake pointers to caller-supplied `$init_fn`.
                unsafe { $init_fn(api, load_ctx, instance_out) }
            })
        }

        /// Tears down and frees a per-World [`BamInstance`]($crate::raw::BamInstance).
        ///
        /// # Safety
        /// `instance` must be either null or a pointer returned by `bevy_abi_mod_init` not yet freed.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn bevy_abi_mod_shutdown(instance: *mut $crate::raw::BamInstance) {
            $crate::raw::catch_guest_unit(|| {
                // SAFETY: Forwarding raw `BamInstance*` to caller-supplied `$shutdown_fn`.
                unsafe { $shutdown_fn(instance) }
            });
        }
    };
}
