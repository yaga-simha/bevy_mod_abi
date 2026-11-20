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
    feature = "bridge-camera-2d",
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
#[cfg(feature = "bridge-camera-2d")]
pub use camera_light::FfiCamera2d;
#[cfg(feature = "bridge-camera-controller")]
pub use camera_light::FfiPanOrbitCamera;
#[cfg(feature = "bridge-solari")]
pub use camera_light::FfiSolariLighting;
#[cfg(any(
    feature = "bridge-camera-2d",
    feature = "bridge-light",
    feature = "bridge-camera-controller",
    feature = "bridge-solari"
))]
pub use camera_light::{
    CameraLightBridgeSystems, ModCameraLightBridgePlugin, register_camera_light_bridges,
};
#[cfg(feature = "bridge-light")]
pub use camera_light::{FfiCamera3d, FfiDirectionalLight, FfiPointLight, FfiSpotLight};
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

/// Curated 2D game preset plugin (`feature = "preset-2d"`) installing `bevy_state::app::StatesPlugin`,
/// [`StdTypesBridgePlugin`], [`ModAssetPlugin`], [`ModPickingBridgePlugin`],
/// [`ModInputExtBridgePlugin`], [`ModAudioBridgePlugin`], [`ModCameraLightBridgePlugin`] (2D
/// camera without 3D PBR/lighting/Solari dependencies), and [`ModUiBridgePlugin`].
///
/// Add this plugin **before** Bevy's `AssetPlugin` so the isolated `"mod"` asset source is
/// registered prior to `AssetServer` construction.
#[cfg(feature = "preset-2d")]
#[derive(Default, Debug, Clone, Copy)]
pub struct ModPreset2dPlugin;

#[cfg(feature = "preset-2d")]
impl bevy_app::Plugin for ModPreset2dPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        if !app.is_plugin_added::<ModBridgesPlugin>() {
            app.add_plugins(ModBridgesPlugin);
        }
        if !app.is_plugin_added::<ModPickingBridgePlugin>() {
            app.add_plugins(ModPickingBridgePlugin);
        }
        if !app.is_plugin_added::<ModInputExtBridgePlugin>() {
            app.add_plugins(ModInputExtBridgePlugin);
        }
        if !app.is_plugin_added::<ModAudioBridgePlugin>() {
            app.add_plugins(ModAudioBridgePlugin);
        }
        if !app.is_plugin_added::<ModCameraLightBridgePlugin>() {
            app.add_plugins(ModCameraLightBridgePlugin);
        }
        if !app.is_plugin_added::<ModUiBridgePlugin>() {
            app.add_plugins(ModUiBridgePlugin);
        }
    }
}

/// Curated 3D game preset plugin (`feature = "preset-3d"`) installing [`ModPreset2dPlugin`]
/// (including 3D cameras, lights, and `PanOrbitCamera` via [`ModCameraLightBridgePlugin`]) plus
/// [`ModBsnPlugin`].
///
/// Add this plugin **before** Bevy's `AssetPlugin` so the isolated `"mod"` asset source is
/// registered prior to `AssetServer` construction.
#[cfg(feature = "preset-3d")]
#[derive(Default, Debug, Clone, Copy)]
pub struct ModPreset3dPlugin;

#[cfg(feature = "preset-3d")]
impl bevy_app::Plugin for ModPreset3dPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        if !app.is_plugin_added::<ModPreset2dPlugin>() {
            app.add_plugins(ModPreset2dPlugin);
        }
        if !app.is_plugin_added::<ModBsnPlugin>() {
            app.add_plugins(ModBsnPlugin);
        }
    }
}

/// Curated UI / editor preset plugin (`feature = "preset-ui"`) installing
/// [`StdTypesBridgePlugin`], [`ModAssetPlugin`], [`ModPickingBridgePlugin`],
/// [`ModInputExtBridgePlugin`], and [`ModUiBridgePlugin`] (with `bevy_feathers` controls).
///
/// Add this plugin **before** Bevy's `AssetPlugin` so the isolated `"mod"` asset source is
/// registered prior to `AssetServer` construction.
#[cfg(feature = "preset-ui")]
#[derive(Default, Debug, Clone, Copy)]
pub struct ModPresetUiPlugin;

#[cfg(feature = "preset-ui")]
impl bevy_app::Plugin for ModPresetUiPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        if !app.is_plugin_added::<StdTypesBridgePlugin>() {
            app.add_plugins(StdTypesBridgePlugin);
        }
        if !app.is_plugin_added::<ModAssetPlugin>() {
            app.add_plugins(ModAssetPlugin);
        }
        if !app.is_plugin_added::<ModPickingBridgePlugin>() {
            app.add_plugins(ModPickingBridgePlugin);
        }
        if !app.is_plugin_added::<ModInputExtBridgePlugin>() {
            app.add_plugins(ModInputExtBridgePlugin);
        }
        if !app.is_plugin_added::<ModUiBridgePlugin>() {
            app.add_plugins(ModUiBridgePlugin);
        }
    }
}

/// Full companion bridge preset plugin (`feature = "preset-full"`) installing all subsystem
/// bridges (3D, UI + Feathers, BSN, Audio, Picking, State, and Solari lighting).
///
/// Add this plugin **before** Bevy's `AssetPlugin` so the isolated `"mod"` asset source is
/// registered prior to `AssetServer` construction.
#[cfg(feature = "preset-full")]
#[derive(Default, Debug, Clone, Copy)]
pub struct ModPresetFullPlugin;

#[cfg(feature = "preset-full")]
impl bevy_app::Plugin for ModPresetFullPlugin {
    fn build(&self, app: &mut bevy_app::App) {
        if !app.is_plugin_added::<ModPreset3dPlugin>() {
            app.add_plugins(ModPreset3dPlugin);
        }
        if !app.is_plugin_added::<ModPresetUiPlugin>() {
            app.add_plugins(ModPresetUiPlugin);
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
    #[cfg(feature = "preset-2d")]
    pub use crate::ModPreset2dPlugin;
    #[cfg(feature = "preset-3d")]
    pub use crate::ModPreset3dPlugin;
    #[cfg(feature = "preset-full")]
    pub use crate::ModPresetFullPlugin;
    #[cfg(feature = "preset-ui")]
    pub use crate::ModPresetUiPlugin;
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
    #[cfg(feature = "bridge-camera-2d")]
    pub use crate::camera_light::FfiCamera2d;
    #[cfg(feature = "bridge-camera-controller")]
    pub use crate::camera_light::FfiPanOrbitCamera;
    #[cfg(feature = "bridge-solari")]
    pub use crate::camera_light::FfiSolariLighting;
    #[cfg(any(
        feature = "bridge-camera-2d",
        feature = "bridge-light",
        feature = "bridge-camera-controller",
        feature = "bridge-solari"
    ))]
    pub use crate::camera_light::{CameraLightBridgeSystems, ModCameraLightBridgePlugin};
    #[cfg(feature = "bridge-light")]
    pub use crate::camera_light::{FfiCamera3d, FfiDirectionalLight, FfiPointLight, FfiSpotLight};
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
