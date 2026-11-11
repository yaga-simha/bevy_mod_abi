//! Modular guest plugin trait and transactional initialization lifecycle helpers.

use bevy_abi_mod_guest_sys::{GuestAppLifecycle, ValidatedInit};
use bevy_abi_mod_sys::FfiResult;

use crate::app::GuestApp;

/// Trait for modular Bevy-like guest plugin definitions exported via `export_mod!(plugin: MyMod)`.
pub trait GuestPlugin {
    /// Registers components, systems, and optional shutdown hooks on `app`.
    fn build(app: &mut GuestApp<'_>) -> FfiResult;

    /// Optional custom teardown callback invoked during `bevy_abi_mod_shutdown` prior to
    /// automatic [`GuestApp`] allocation cleanup.
    fn shutdown() -> FfiResult {
        FfiResult::OK
    }
}

impl GuestAppLifecycle for GuestApp<'static> {
    type App<'a> = GuestApp<'a>;

    #[inline]
    fn build_from_validated<'a>(init: ValidatedInit<'a>) -> Self::App<'a> {
        GuestApp::from_validated(init)
    }

    #[inline]
    fn commit_app(app: Self::App<'_>) {
        app.commit();
    }
}

/// Runs a [`GuestApp`] setup closure over an already-validated [`ValidatedInit`] session,
/// committing staged allocations on [`FfiResult::OK`] and rolling them back otherwise.
pub fn run_validated_guest_app<F>(init: ValidatedInit<'_>, build_fn: F) -> FfiResult
where
    F: FnOnce(&mut GuestApp<'_>) -> FfiResult,
{
    let mut app = GuestApp::from_validated(init);
    let status = build_fn(&mut app);
    if status.is_ok() {
        app.commit();
    }
    status
}
