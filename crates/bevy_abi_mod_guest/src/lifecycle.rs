//! Per-instance `GuestPlugin` trait and `GuestApp` initialization helpers.

use core::ffi::CStr;
use std::sync::Arc;

use bevy_abi_mod_guest_sys::{
    GuestInstance, ReentrantCell, ValidatedLoadCtx, guest_restore_state_trampoline,
    guest_save_state_trampoline,
};
use bevy_abi_mod_sys::{BamManifest, FfiResult};

use crate::app::GuestApp;

/// Idiomatic per-World guest plugin trait exported via [`export_mod!(plugin: MyMod)`](crate::export_mod).
///
/// Each Bevy `World` that loads your `.cdylib` instantiates a fresh `Self::default()` inside its
/// heap-allocated [`GuestInstance`], invokes [`Self::build`], optionally transfers state across
/// hot-reloads via [`Self::save_state`] and [`Self::restore_state`], and invokes
/// [`Self::on_shutdown`] when that specific load session unloads—sharing zero process-global
/// mutable state across worlds.
pub trait GuestPlugin: Default + Send + 'static {
    /// Canonical NUL-terminated UTF-8 mod identifier (e.g. `c"combat_mod"`).
    const NAME: &'static CStr;

    /// NUL-terminated UTF-8 SemVer mod version string (defaults to `c"0.1.0"`).
    const VERSION: &'static CStr = c"0.1.0";

    /// Optional `BAM_MANIFEST_*` bitflags (`0` in ABI `0.2`).
    const FLAGS: u32 = 0;

    /// Compile-time [`BamManifest`] returned by `bevy_abi_mod_manifest()`.
    const MANIFEST: BamManifest = BamManifest::new(Self::NAME.as_ptr(), Self::VERSION.as_ptr())
        .with_flags(Self::FLAGS)
        .with_state_callbacks(guest_save_state_trampoline, guest_restore_state_trampoline);

    /// Registers components, systems, and per-instance hooks on `app`.
    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult;

    /// Optional hot-reload state snapshot callback invoked prior to `bevy_abi_mod_shutdown`.
    ///
    /// Return serialized bytes to transfer into the replacement instance's [`Self::restore_state`].
    fn save_state(&self) -> Result<Vec<u8>, FfiResult> {
        Ok(Vec::new())
    }

    /// Optional hot-reload state restoration callback invoked immediately after [`Self::build`]
    /// during `reload_mod`.
    fn restore_state(&mut self, _bytes: &[u8]) -> FfiResult {
        FfiResult::OK
    }

    /// Optional per-instance teardown callback invoked during `bevy_abi_mod_shutdown` prior to
    /// freeing registered system closures.
    fn on_shutdown(&mut self) {}
}

/// Runs a [`GuestPlugin`] lifecycle build step inside `instance`, attaching `P::save_state`,
/// `P::restore_state`, and `P::on_shutdown` as per-instance hooks when [`GuestPlugin::build`]
/// returns [`FfiResult::OK`].
///
/// Note that calling [`GuestApp::on_save_state`] or [`GuestApp::on_restore_state`] inside
/// [`GuestPlugin::build`] overrides the default trait-method state hooks for that instance.
pub fn build_guest_plugin<P: GuestPlugin>(
    instance: &mut GuestInstance,
    load_ctx: ValidatedLoadCtx<'_>,
) -> FfiResult {
    let plugin_cell = Arc::new(ReentrantCell::new(P::default()));
    {
        let save_cell = Arc::clone(&plugin_cell);
        instance.set_save_state_hook(move || {
            save_cell
                .try_with_mut(|p| p.save_state())
                .unwrap_or(Err(FfiResult::ACCESS_CONFLICT))
        });
        let restore_cell = Arc::clone(&plugin_cell);
        instance.set_restore_state_hook(move |bytes| {
            restore_cell
                .try_with_mut(|p| p.restore_state(bytes))
                .unwrap_or(FfiResult::ACCESS_CONFLICT)
        });
    }
    let status = plugin_cell
        .try_with_mut(|plugin| {
            let mut app = GuestApp::from_validated(instance, load_ctx);
            plugin.build(&mut app)
        })
        .unwrap_or(FfiResult::ACCESS_CONFLICT);
    if status.is_ok() {
        instance.add_shutdown_hook(move || {
            let _ = plugin_cell.try_with_mut(|plugin| {
                plugin.on_shutdown();
            });
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
