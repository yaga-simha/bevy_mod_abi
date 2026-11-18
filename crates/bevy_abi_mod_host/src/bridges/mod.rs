//! Safe (`#![forbid(unsafe_code)]`) host bridges for Bevy standard components, resources,
//! isolated mod assets, and Bevy states.

#[cfg(feature = "bridge-asset")]
pub mod asset;
#[cfg(feature = "bridge-bsn")]
pub mod bsn;
#[cfg(feature = "bridge-reflect")]
pub mod reflect;
#[cfg(feature = "bridge-state")]
pub mod state;
#[cfg(feature = "bridge-std-types")]
pub mod std_types;

#[cfg(feature = "bridge-asset")]
pub use asset::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
#[cfg(feature = "bridge-state")]
pub use state::{ModNextState, ModStateError, ModStateExt, ModStateSpec};
#[cfg(feature = "bridge-std-types")]
pub use std_types::{
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, StdTypesBridgePlugin,
    register_std_types_bridges,
};

/// Convenience umbrella plugin installing [`StdTypesBridgePlugin`], [`ModAssetPlugin`], and
/// `bevy_state::app::StatesPlugin` (when all three bridge features are enabled).
///
/// Add this plugin **before** Bevy's `AssetPlugin` so the isolated `"mod"` asset source is
/// registered prior to `AssetServer` construction.
#[cfg(all(
    feature = "bridge-std-types",
    feature = "bridge-asset",
    feature = "bridge-state"
))]
#[derive(Default, Debug, Clone, Copy)]
pub struct ModBridgesPlugin;

#[cfg(all(
    feature = "bridge-std-types",
    feature = "bridge-asset",
    feature = "bridge-state"
))]
impl bevy_app::Plugin for ModBridgesPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        if !app.is_plugin_added::<bevy_state::app::StatesPlugin>() {
            app.add_plugins(bevy_state::app::StatesPlugin);
        }
        if !app.is_plugin_added::<StdTypesBridgePlugin>() {
            app.add_plugins(StdTypesBridgePlugin);
        }
        if !app.is_plugin_added::<ModAssetPlugin>() {
            app.add_plugins(ModAssetPlugin);
        }
    }
}
