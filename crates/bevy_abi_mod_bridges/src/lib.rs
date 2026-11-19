#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Safe (`#![forbid(unsafe_code)]`) optional Bevy subsystem bridges for `bevy_abi_mod` (`ABI 0.3.0`).
//!
//! Each subsystem bridge is individually feature-gated so headless hosts do not pay compile-time
//! or dependency costs for subsystems they do not use:
//!
//! - `bridge-std-types`: Zero-copy / VTable host bridges for `Transform`, `GlobalTransform`,
//!   `Visibility`, `Name`, `Time<Virtual|Fixed|Real>`, `ButtonInput<KeyCode>`, and
//!   `ButtonInput<MouseButton>` (`StdTypesBridgePlugin`).
//! - `bridge-asset`: Isolated `mod://<namespace>/...` Bevy `AssetSource` + refcounted
//!   `FfiAssetHandle` backend (`ModAssetPlugin`).
//! - `bridge-state`: Bevy `States` exposure (`ModStateExt::expose_mod_state`), transition
//!   schedules, `in_state` run conditions, and `DespawnOnExit`/`DespawnOnEnter` markers.
//! - `bridge-bsn`: Bevy Scene Notation (`bevy_scene`) spawning (`cmd_spawn_scene`) and
//!   `bevy_scene::Ready` observer integration (`ModBsnPlugin`).
//! - `bridge-picking`: Flattened Bevy `0.20` `bevy_picking` observer event bridge, `EntityCursor`
//!   sync (`ModPickingBridgePlugin`), and extended mouse motion/scroll + `Gamepad` input & rumble
//!   bridges (`ModInputExtBridgePlugin`).

#[cfg(feature = "bridge-asset")]
pub mod asset;
#[cfg(feature = "bridge-bsn")]
pub mod bsn;
#[cfg(any(feature = "bridge-picking", feature = "bridge-std-types"))]
pub mod input_ext;
#[cfg(feature = "bridge-picking")]
pub mod picking;
#[cfg(feature = "bridge-state")]
pub mod state;
#[cfg(feature = "bridge-std-types")]
pub mod std_types;

#[cfg(feature = "bridge-asset")]
pub use asset::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
#[cfg(feature = "bridge-bsn")]
pub use bsn::{
    CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt,
    SCENE_PATCH_ALIAS,
};
#[cfg(feature = "bridge-picking")]
pub use input_ext::{FfiGamepadButton, FfiGamepadRumble, FfiGamepadState};
#[cfg(any(feature = "bridge-picking", feature = "bridge-std-types"))]
pub use input_ext::{
    FfiMouseMotion, FfiMouseScroll, InputExtBridgeSet, ModInputExtBridgePlugin,
    register_input_ext_bridges,
};
#[cfg(feature = "bridge-picking")]
pub use picking::{
    FfiEntityCursor, FfiPointerButton, FfiPointerClick, FfiPointerDrag, FfiPointerKind,
    FfiPointerMove, FfiPointerOut, FfiPointerOver, FfiPointerPress, FfiPointerRelease,
    ModPickingBridgePlugin, PickingBridgeSet, register_picking_bridges,
};
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

/// Convenient prelude re-exporting enabled Bevy subsystem bridge plugins, traits, and FFI types.
pub mod prelude {
    #[cfg(all(
        feature = "bridge-std-types",
        feature = "bridge-asset",
        feature = "bridge-state"
    ))]
    pub use crate::ModBridgesPlugin;
    #[cfg(feature = "bridge-asset")]
    pub use crate::asset::{ModAssetAppExt, ModAssetPlugin, ModAssetRegistry, ModAssetWorldExt};
    #[cfg(feature = "bridge-bsn")]
    pub use crate::bsn::{HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt};
    #[cfg(feature = "bridge-picking")]
    pub use crate::input_ext::{FfiGamepadButton, FfiGamepadRumble, FfiGamepadState};
    #[cfg(any(feature = "bridge-picking", feature = "bridge-std-types"))]
    pub use crate::input_ext::{
        FfiMouseMotion, FfiMouseScroll, InputExtBridgeSet, ModInputExtBridgePlugin,
    };
    #[cfg(feature = "bridge-picking")]
    pub use crate::picking::{
        FfiEntityCursor, FfiPointerButton, FfiPointerClick, FfiPointerDrag, FfiPointerKind,
        FfiPointerMove, FfiPointerOut, FfiPointerOver, FfiPointerPress, FfiPointerRelease,
        ModPickingBridgePlugin, PickingBridgeSet,
    };
    #[cfg(feature = "bridge-state")]
    pub use crate::state::{ModNextState, ModStateError, ModStateExt, ModStateSpec};
    #[cfg(feature = "bridge-std-types")]
    pub use crate::std_types::{
        FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput,
        FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility,
        StdTypesBridgePlugin,
    };
}
