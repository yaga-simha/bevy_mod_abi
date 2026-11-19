#![forbid(unsafe_code)]

//! Verifies that the top-level `bevy_abi_mod` facade preserves all existing public imports,
//! module paths (`bridges::*`, `sub_app_materials`, `watcher`), and `prelude::*` exports across
//! the split host crates (`bevy_abi_mod_host`, `bevy_abi_mod_bridges`, `bevy_abi_mod_render`,
//! `bevy_abi_mod_reflect`, `bevy_abi_mod_watcher`).

use bevy_abi_mod::prelude::*;
use bevy_app::App;

#[test]
fn facade_core_host_exports_work() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    assert!(app.world().contains_resource::<ModHost>());
    assert!(app.world().contains_resource::<ModRegistry>());
    assert!(app.world().contains_resource::<ModUnloadHooks>());
    assert!(app.world().contains_resource::<ModBudgetPolicy>());
    assert!(app.world().contains_resource::<ModBudgetTelemetry>());
    let _ = core::any::type_name::<bevy_abi_mod::budgets::ModBudgetLimits>();
}

#[cfg(feature = "bridge-std-types")]
#[test]
fn facade_std_types_reexports_work() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, bevy_abi_mod::bridges::StdTypesBridgePlugin));
    let _t = bevy_abi_mod::FfiTransform::IDENTITY;
    let _gt = bevy_abi_mod::bridges::std_types::FfiGlobalTransform::IDENTITY;
}

#[cfg(feature = "bridge-asset")]
#[test]
fn facade_asset_reexports_work() {
    let _plugin = bevy_abi_mod::ModAssetPlugin;
    let _reg = bevy_abi_mod::bridges::asset::ModAssetRegistry::new();
}

#[cfg(feature = "bridge-state")]
#[test]
fn facade_state_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModStateError>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::state::ModStateError>();
}

#[cfg(feature = "bridge-bsn")]
#[test]
fn facade_bsn_reexports_work() {
    assert_eq!(
        bevy_abi_mod::CANONICAL_READY_NAME,
        bevy_abi_mod::bridges::bsn::CANONICAL_READY_NAME
    );
    let _reg = bevy_abi_mod::HostSceneRegistry::new();
}

#[cfg(feature = "bridge-reflect")]
#[test]
fn facade_reflect_reexports_work() {
    assert_eq!(
        bevy_abi_mod::BRP_METHOD_LIST_COMPONENTS,
        bevy_abi_mod::bridges::reflect::BRP_METHOD_LIST_COMPONENTS
    );
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModReflectPlugin));
    assert!(app.world().contains_resource::<ModReflectRegistry>());
}

#[cfg(feature = "sub-app-extract")]
#[test]
fn facade_render_reexports_work() {
    let mut app = App::new();
    app.add_plugins((AbiModPlugin, ModWeslMaterialPlugin));
    assert!(app.world().contains_resource::<ModWeslMaterialRegistry>());
    let _spec = bevy_abi_mod::sub_app_materials::ModSpriteMaterialSpec::new("smoke");
    let _params = ModMaterialParams::default();
    let _ext = ModSpriteExtension2d::default();
    assert_eq!(app.cleanup_mod_materials(ModId(999_999)), 0);
}

#[cfg(feature = "hot-reload-watcher")]
#[test]
fn facade_watcher_reexports_work() {
    let _history = bevy_abi_mod::ModHotReloadHistory::default();
    let _action = bevy_abi_mod::watcher::ModHotReloadAction::Loaded;
}

#[cfg(feature = "bridge-picking")]
#[test]
fn facade_picking_and_input_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModPickingBridgePlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::picking::FfiPointerClick>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::input_ext::FfiGamepadState>();
}

#[cfg(feature = "bridge-audio")]
#[test]
fn facade_audio_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModAudioBridgePlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::audio::FfiAudioPlayerSpec>();
}

#[cfg(feature = "bridge-light")]
#[test]
fn facade_camera_light_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModCameraLightBridgePlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::camera_light::FfiCamera3d>();
}

#[cfg(feature = "bridge-ui")]
#[test]
fn facade_ui_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModUiBridgePlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::bridges::ui::FfiUiNode>();
}

#[cfg(feature = "bridge-feathers")]
#[test]
fn facade_feathers_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::FfiThemeContext>();
    let _ = core::any::type_name::<bevy_abi_mod::FfiFeathersNumberInput>();
}

#[cfg(feature = "bridge-camera-controller")]
#[test]
fn facade_camera_controller_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::FfiPanOrbitCamera>();
}

#[cfg(feature = "bridge-solari")]
#[test]
fn facade_solari_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::FfiSolariLighting>();
}

#[cfg(feature = "bridge-reflect")]
#[test]
fn facade_savegame_and_telemetry_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModSaveGamePlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::savegame::ModSaveArchive>();
    let _ = core::any::type_name::<bevy_abi_mod::ModTelemetryPlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::telemetry::SystemTelemetryDigest64>();
}

#[cfg(feature = "sub-app-extract")]
#[test]
fn facade_sprite_mesh_shaders_reexports_work() {
    let _ = core::any::type_name::<bevy_abi_mod::ModSpriteMaterialPlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::ModMeshShaderPlugin>();
    let _ = core::any::type_name::<bevy_abi_mod::sprite_mesh_shaders::ModUnifiedSpriteSpec>();
}
