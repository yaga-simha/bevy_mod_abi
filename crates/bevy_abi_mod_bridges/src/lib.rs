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
#[cfg(feature = "bridge-audio")]
pub mod audio;
#[cfg(feature = "bridge-bsn")]
pub mod bsn;
#[cfg(any(
    feature = "bridge-light",
    feature = "bridge-camera-controller",
    feature = "bridge-solari"
))]
pub mod camera_light;
#[cfg(any(feature = "bridge-picking", feature = "bridge-std-types"))]
pub mod input_ext;
#[cfg(feature = "bridge-picking")]
pub mod picking;
#[cfg(feature = "bridge-state")]
pub mod state;
#[cfg(feature = "bridge-std-types")]
pub mod std_types;
#[cfg(feature = "bridge-ui")]
pub mod ui;

#[cfg(feature = "bridge-asset")]
pub use asset::{
    AssetOverlayPolicy, ConfiguredModAssetPlugin, ModAssetAppExt, ModAssetPlugin, ModAssetRegistry,
    ModAssetWorldExt, OVERLAY_NAMESPACE,
};
#[cfg(feature = "bridge-audio")]
pub use audio::{
    AudioBridgeSystems, FfiAudioPlaybackSettings, FfiAudioPlayerSpec, FfiSpatialAudioSinkControl,
    ModAudioBridgePlugin, register_audio_bridges,
};
#[cfg(feature = "bridge-bsn")]
pub use bsn::{
    CANONICAL_READY_NAME, HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt,
    SCENE_PATCH_ALIAS,
};
#[cfg(feature = "bridge-camera-controller")]
pub use camera_light::FfiPanOrbitCamera;
#[cfg(feature = "bridge-solari")]
pub use camera_light::FfiSolariLighting;
#[cfg(any(
    feature = "bridge-light",
    feature = "bridge-camera-controller",
    feature = "bridge-solari"
))]
pub use camera_light::{
    CameraLightBridgeSystems, ModCameraLightBridgePlugin, register_camera_light_bridges,
};
#[cfg(feature = "bridge-light")]
pub use camera_light::{
    FfiCamera2d, FfiCamera3d, FfiDirectionalLight, FfiPointLight, FfiSpotLight,
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
pub use state::{
    ModDespawnOnEnter, ModDespawnOnExit, ModNextState, ModStateEntry, ModStateError, ModStateExt,
    ModStateHandle, ModStateKind, ModStateSlab, ModStateSpec, ModStateTransitionRequest,
    STATE_ABSENT_DISCRIMINANT,
};
#[cfg(feature = "bridge-std-types")]
pub use std_types::{
    CANONICAL_DISABLED_NAME, FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton,
    FfiMouseInput, FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility,
    StdTypesBridgePlugin, register_std_types_bridges,
};
#[cfg(feature = "bridge-ui")]
pub use ui::{
    FfiBorderColor, FfiBorderRadius, FfiCornerRadius, FfiEditableTextConfig, FfiEmSize,
    FfiFontSize, FfiInlineBox, FfiInlineImage, FfiRemSize, FfiRgba, FfiSelectedTab, FfiTab,
    FfiTabList, FfiUiNode, FfiUiRect, FfiUiTextStyle, FfiVal, FfiValueChange, ModUiAppExt,
    ModUiBridgePlugin, ModUiBridgeSet, ModUiRegistry, ModUiWorldExt, cleanup_mod_ui_in_world,
    register_ui_bridges,
};
#[cfg(feature = "bridge-feathers")]
pub use ui::{
    FfiFeathersColorInput, FfiFeathersDropdown, FfiFeathersListRow, FfiFeathersListView,
    FfiFeathersNumberInput, FfiFeathersScrollableList, FfiFeathersSelect, FfiThemeContext,
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
    pub use crate::asset::{
        AssetOverlayPolicy, ConfiguredModAssetPlugin, ModAssetAppExt, ModAssetPlugin,
        ModAssetRegistry, ModAssetWorldExt, OVERLAY_NAMESPACE,
    };
    #[cfg(feature = "bridge-audio")]
    pub use crate::audio::{
        AudioBridgeSystems, FfiAudioPlaybackSettings, FfiAudioPlayerSpec,
        FfiSpatialAudioSinkControl, ModAudioBridgePlugin,
    };
    #[cfg(feature = "bridge-bsn")]
    pub use crate::bsn::{HostSceneRegistry, ModBsnAppExt, ModBsnPlugin, ModBsnWorldExt};
    #[cfg(feature = "bridge-camera-controller")]
    pub use crate::camera_light::FfiPanOrbitCamera;
    #[cfg(feature = "bridge-solari")]
    pub use crate::camera_light::FfiSolariLighting;
    #[cfg(any(
        feature = "bridge-light",
        feature = "bridge-camera-controller",
        feature = "bridge-solari"
    ))]
    pub use crate::camera_light::{CameraLightBridgeSystems, ModCameraLightBridgePlugin};
    #[cfg(feature = "bridge-light")]
    pub use crate::camera_light::{
        FfiCamera2d, FfiCamera3d, FfiDirectionalLight, FfiPointLight, FfiSpotLight,
    };
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
    pub use crate::state::{
        ModDespawnOnEnter, ModDespawnOnExit, ModNextState, ModStateEntry, ModStateError,
        ModStateExt, ModStateHandle, ModStateKind, ModStateSlab, ModStateSpec,
        ModStateTransitionRequest, STATE_ABSENT_DISCRIMINANT,
    };
    #[cfg(feature = "bridge-std-types")]
    pub use crate::std_types::{
        CANONICAL_DISABLED_NAME, FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton,
        FfiMouseInput, FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility,
        StdTypesBridgePlugin,
    };
    #[cfg(feature = "bridge-ui")]
    pub use crate::ui::{
        FfiBorderColor, FfiBorderRadius, FfiCornerRadius, FfiEditableTextConfig, FfiEmSize,
        FfiFontSize, FfiInlineBox, FfiInlineImage, FfiRemSize, FfiRgba, FfiSelectedTab, FfiTab,
        FfiTabList, FfiUiNode, FfiUiRect, FfiUiTextStyle, FfiVal, FfiValueChange, ModUiAppExt,
        ModUiBridgePlugin, ModUiBridgeSet, ModUiRegistry, ModUiWorldExt,
    };
    #[cfg(feature = "bridge-feathers")]
    pub use crate::ui::{
        FfiFeathersColorInput, FfiFeathersDropdown, FfiFeathersListRow, FfiFeathersListView,
        FfiFeathersNumberInput, FfiFeathersScrollableList, FfiFeathersSelect, FfiThemeContext,
    };
}
