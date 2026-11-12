//! Per-instance `GuestPlugin` trait and `GuestApp` initialization helpers (`L-02`, `L-08`).

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{GuestInstance, ValidatedLoadCtx};
use bevy_abi_mod_sys::{BamManifest, FfiResult};

use crate::app::GuestApp;

/// Idiomatic per-World guest plugin trait exported via [`export_mod!(plugin: MyMod)`](crate::export_mod).
///
/// Each Bevy `World` that loads your `.cdylib` instantiates a fresh `Self::default()` inside its
/// heap-allocated [`GuestInstance`], invokes [`Self::build`], and later invokes [`Self::on_shutdown`]
/// when that specific load session unloads—sharing zero process-global mutable state across worlds.
pub trait GuestPlugin: Default + Send + 'static {
    /// Canonical NUL-terminated UTF-8 mod identifier (e.g. `c"combat_mod"`).
    const NAME: &'static CStr;

    /// NUL-terminated UTF-8 SemVer mod version string (defaults to `c"0.1.0"`).
    const VERSION: &'static CStr = c"0.1.0";

    /// Optional `BAM_MANIFEST_*` bitflags (`0` in ABI `0.2`).
    const FLAGS: u32 = 0;

    /// Compile-time [`BamManifest`] returned by `bevy_abi_mod_manifest()`.
    const MANIFEST: BamManifest =
        BamManifest::new(Self::NAME.as_ptr(), Self::VERSION.as_ptr()).with_flags(Self::FLAGS);

    /// Registers components, systems, and per-instance hooks on `app`.
    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult;

    /// Optional per-instance teardown callback invoked during `bevy_abi_mod_shutdown` prior to
    /// freeing registered system closures.
    fn on_shutdown(&mut self) {}
}

/// Runs a [`GuestPlugin`] lifecycle build step inside `instance`, attaching `P::on_shutdown` as a
/// per-instance shutdown hook when [`GuestPlugin::build`] returns [`FfiResult::OK`].
pub fn build_guest_plugin<P: GuestPlugin>(
    instance: &mut GuestInstance,
    load_ctx: ValidatedLoadCtx<'_>,
) -> FfiResult {
    let mut plugin = P::default();
    let status = {
        let mut app = GuestApp::from_validated(instance, load_ctx);
        plugin.build(&mut app)
    };
    if status.is_ok() {
        instance.add_shutdown_hook(move || {
            plugin.on_shutdown();
        });
    }
    status
}

/// Runs a stateless setup function `fn(&mut GuestApp<'_>) -> FfiResult` inside `instance`.
pub fn build_guest_fn<F>(
    instance: &mut GuestInstance,
    load_ctx: ValidatedLoadCtx<'_>,
    build_fn: F,
) -> FfiResult
where
    F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
{
    let mut app = GuestApp::from_validated(instance, load_ctx);
    build_fn(&mut app)
}
