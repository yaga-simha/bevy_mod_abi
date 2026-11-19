#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Showcase Bevy `0.20.1` full companion modding demo (`10_bevy20_full_companion`).
//!
//! Combines all Phase 3 companion capabilities in a single cohesive, `#![forbid(unsafe_code)]`,
//! headless-testable simulation (`build_companion_app(headless: bool)`):
//!
//! 1. **3D Camera, Lights & `PanOrbitCamera`** (`ModCameraLightBridgePlugin`: `FfiCamera3d`,
//!    `FfiPanOrbitCamera`, `FfiDirectionalLight`, `FfiPointLight` pulsed live by Mod A).
//! 2. **Custom `.wesl` `SpriteMaterial` & `ExtendedMaterial2d`** (`ModWeslMaterialPlugin`,
//!    `ModSpriteMaterialPlugin`, `ModSubAppPlugin::<RenderApp>` registering a real `.wesl`
//!    shader, aligned `ShaderBuffer`, unified `SpriteMaterial`, and `ModExtendedSpriteMaterial2d`
//!    with headless specialization and `RenderApp` extraction).
//! 3. **Flattened `PointerPress` Picking Observers** (`ModPickingBridgePlugin`: synthetic Bevy
//!    `0.20` `PointerPress` forwarded into a real guest `cmd_observe` handler mutating entity
//!    state).
//! 4. **Spatial `AudioPlayer`** (`ModAudioBridgePlugin`: `FfiAudioPlayerSpec` +
//!    `FfiAudioPlaybackSettings` materializing `AudioPlayer<AudioSource>` + `PlaybackSettings`
//!    headlessly without an OS audio device).
//! 5. **Retained UI (`Val::Rem` + Headless `TabList` HUD + Feathers `ThemeContext`)**
//!    (`ModUiBridgePlugin`: `FfiUiNode` with `Val::Rem` + `FixedNode`, `FfiFontSize::Rem`,
//!    `FfiTabList` / `FfiTab` / `FfiSelectedTab`, `FfiThemeContext::FLOATING`, and a guest
//!    `cmd_observe` observer handling `FfiValueChange`).
//! 6. **`ModBudgetPolicy` + `ModTelemetryPlugin` Live Telemetry** (`ModBudgetPolicy`,
//!    `ModBudgetTelemetry`, `DiagnosticsStore`, and live `RemoteMethods` BRP queries for
//!    `bam/list_mods`, `bam/system_metrics`, and `bam/component_schemas`).
//! 7. **`ModSaveGamePlugin` Quick-Save / Mod-Disable / Quick-Load Recovery** (persists Host +
//!    Mod A + Mod B components while excluding ephemeral `ModScoped` entities, verifies
//!    `StrictRequireMods` rejects missing Mod B atomically, and recovers cleanly via
//!    `StripMissingModComponents` preserving Host + Mod A state byte-for-byte).

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

use bevy_abi_mod::bridges::audio::{
    AUDIO_SOURCE_ASSET_ALIAS, PLAYBACK_MODE_LOOP, PLAYBACK_MODE_ONCE, audio_source_handle_to_ffi,
};
use bevy_abi_mod::bridges::camera_light::{
    CLEAR_COLOR_CUSTOM, ORBIT_CONSTRAINT_FIXED, TONEMAPPING_AGX, TRANSMISSION_QUALITY_HIGH,
};
use bevy_abi_mod::prelude::*;
use bevy_abi_mod::{
    BRP_METHOD_BAM_COMPONENT_SCHEMAS, BRP_METHOD_BAM_LIST_MODS, BRP_METHOD_BAM_SYSTEM_METRICS,
    build_quad_mesh_2d, read_entity_pod_bytes,
};
use bevy_app::{App, TaskPoolPlugin};
use bevy_asset::{AssetPlugin, Assets, Handle};
use bevy_audio::{AudioPlayer, AudioSource, PlaybackMode, PlaybackSettings, Volume};
use bevy_camera::{Camera, Camera3d, Hdr, NormalizedRenderTarget};
use bevy_camera_controller::pan_orbit_camera::controller::component::PanOrbitCamera;
use bevy_diagnostic::DiagnosticsStore;
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::message::Messages;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::Resource;
use bevy_ecs::world::World;
use bevy_extract::sync_world::MainEntity;
use bevy_feathers::theme::{SurfaceLevel, ThemeContext};
use bevy_input::InputPlugin;
use bevy_light::{DirectionalLight, PointLight};
use bevy_material::descriptor::{BindGroupLayoutDescriptor, RenderPipelineDescriptor};
use bevy_math::{Vec2, Vec3};
use bevy_pbr::ScreenSpaceTransmission;
use bevy_picking::backend::HitData;
use bevy_picking::events::{Pointer, PointerPress};
use bevy_picking::pointer::{Location, PointerButton, PointerId};
use bevy_remote::{BrpResult, RemoteMethodSystemId, RemoteMethods, RemotePlugin};
use bevy_render::RenderApp;
use bevy_render::camera::DirtySpecializations;
use bevy_render::mesh::{BaseMeshPipelineKey, Mesh, Mesh2d, MeshVertexBufferLayouts};
use bevy_render::render_resource::{AsBindGroup, SpecializedMeshPipeline, TextureFormat};
use bevy_render::storage::ShaderBuffer;
use bevy_render::view::{DebandDither, Tonemapping};
use bevy_shader::{Shader, ShaderDefVal, Source};
use bevy_sprite::{Sprite, SpriteAlphaMode};
use bevy_sprite_render::{
    AlphaMode2d, EntitiesNeedingSpecialization, Material2d, Material2dKey, Material2dPipeline,
    MaterialExtension2d, Mesh2dPipeline, Mesh2dPipelineKey, MeshMaterial2d, SpriteMaterial,
    SpriteMeshMaterial,
};
use bevy_text::{FontSize, RemSize, TextFont};
use bevy_transform::components::Transform;
use bevy_ui::{FixedNode, Node, Selected, Val};
use bevy_ui_widgets::{SelectedTab, Tab, TabList, ValueChange};
use serde_json::Value;

/// Embedded `.wesl` shader source loaded from `assets/shaders/companion_pulse.wesl`.
pub const COMPANION_PULSE_WESL: &str = include_str!("../assets/shaders/companion_pulse.wesl");

/// Canonical ABI name of Mod A (`companion_core_mod`)'s dynamic component.
pub const MOD_A_ORBITAL_BEACON_NAME: &str = "companion_core_mod::OrbitalBeacon";

/// Canonical ABI name of Mod B (`companion_fx_mod`)'s dynamic component.
pub const MOD_B_PLASMA_AURA_NAME: &str = "companion_fx_mod::PlasmaAura";

/// Host runtime mode resource recorded by [`build_companion_app`].
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompanionRunConfig {
    /// Whether the app is configured for deterministic headless execution (no window/GPU).
    pub headless: bool,
}

/// Persistent host ECS component shared across the C-ABI with both guest mods.
///
/// Layout: 24 bytes, 4-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "companion::Beacon")]
pub struct CompanionBeacon {
    /// Stable application-assigned beacon identifier (`1`, `2`, `3`, ...).
    pub beacon_id: u32,
    /// Cumulative `PointerPress` press count observed via guest `cmd_observe`.
    pub press_count: u32,
    /// Raw `FfiPointerButton` discriminant from the most recent `PointerPress`.
    pub last_button: u32,
    /// Live energy level incremented by ticks and boosted by pointer presses.
    pub energy: f32,
    /// Most recently selected HUD tab index recorded by the `FfiValueChange` guest observer.
    pub active_tab_index: u32,
    /// Cumulative `FfiValueChange` UI events handled by the guest observer.
    pub ui_events: u32,
}

/// Built native shared-library paths for Mod A (`companion_core_mod`) and Mod B (`companion_fx_mod`).
#[derive(Clone, Debug)]
pub struct CompanionGuestMods {
    /// Path to compiled Mod A (`companion_core_mod`) dynamic library.
    pub mod_a_path: PathBuf,
    /// Path to compiled Mod B (`companion_fx_mod`) dynamic library.
    pub mod_b_path: PathBuf,
}

/// Entity and asset handles spawned into a live companion studio scene.
#[derive(Clone, Debug)]
pub struct CompanionSceneHandles {
    /// Three persistent (`!ModScoped`) beacon entities carrying Host + Mod A + Mod B components.
    pub persistent_beacons: [Entity; 3],
    /// `ModScoped` 3D camera + `PanOrbitCamera` rig entity.
    pub camera_rig: Entity,
    /// `ModScoped` directional sun light entity.
    pub directional_light: Entity,
    /// `ModScoped` point light entity animated live by Mod A.
    pub point_light: Entity,
    /// `ModScoped` spatial audio emitter entity.
    pub spatial_audio_emitter: Entity,
    /// `ModScoped` retained HUD root panel (`Val::Rem` + `FixedNode` + Feathers `ThemeContext`).
    pub hud_root: Entity,
    /// `ModScoped` retained HUD title text (`FontSize::Rem`).
    pub hud_title: Entity,
    /// `ModScoped` headless `TabList` entity.
    pub hud_tablist: Entity,
    /// `ModScoped` first tab (`Overview`).
    pub tab_overview: Entity,
    /// `ModScoped` second tab (`Telemetry`).
    pub tab_telemetry: Entity,
    /// `ModScoped` unified `Sprite` + `Mesh2d` + `SpriteMaterial<ModWeslSpriteExtension2d>` entity.
    pub unified_sprite: Entity,
    /// `ModScoped` `ExtendedMaterial2d<SpriteMeshMaterial, ModWeslSpriteExtension2d>` entity.
    pub extended_sprite: Entity,
    /// Registered `.wesl` shader handle owned by Mod A.
    pub wesl_shader: Handle<Shader>,
    /// Registered aligned `ShaderBuffer` handle owned by Mod A.
    pub shader_buffer: Handle<ShaderBuffer>,
    /// Registered `ModWeslSpriteExtension2d` handle owned by Mod A.
    pub sprite_extension: Handle<ModWeslSpriteExtension2d>,
    /// Registered `ModExtendedSpriteMaterial2d` handle owned by Mod A.
    pub extended_material: Handle<ModExtendedSpriteMaterial2d>,
    /// Synthetic audio clip handle for the spatial audio emitter.
    pub audio_clip: Handle<AudioSource>,
}

/// Structured verification report returned by [`run_companion_demo`].
#[derive(Clone, Debug)]
pub struct CompanionDemoReport {
    /// Whether the demo ran in headless mode.
    pub headless: bool,
    /// Final `PointLight::intensity` synced from Mod A's guest mutations.
    pub synced_point_light_intensity: f32,
    /// Final `CompanionBeacon` state on `beacon_alpha` after `PointerPress` + `ValueChange`.
    pub primary_beacon_after_events: CompanionBeacon,
    /// Number of active mods reported by BRP `bam/list_mods`.
    pub brp_total_mods: u64,
    /// Number of tracked guest systems reported by BRP `bam/system_metrics`.
    pub brp_tracked_systems: usize,
    /// Live per-system `(mod_name::system_name, p50_us, p99_us)` metrics from `bam/system_metrics`.
    pub system_metrics_us: Vec<(String, u64, u64)>,
    /// Number of persistent entities serialized into the Quick-Save archive.
    pub saved_persistent_entities: usize,
    /// Number of persistent entities restored during `StripMissingModComponents` Quick-Load.
    pub recovered_entities: usize,
    /// Number of disabled Mod B components cleanly stripped during Quick-Load.
    pub stripped_mod_b_components: usize,
    /// Canonical names of stripped components during Quick-Load.
    pub stripped_component_names: Vec<String>,
    /// Cumulative `OrbitalBeacon::ticks` on `beacon_alpha` after resuming post-recovery simulation.
    pub post_recovery_orbital_ticks: u32,
}

fn dylib_naming() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

fn build_guest_variant(guest_manifest: &Path, target_dir: &Path, extra_args: &[&str]) -> PathBuf {
    let mut cmd = Command::new("cargo");
    cmd.arg("build")
        .arg("--manifest-path")
        .arg(guest_manifest)
        .arg("--release")
        .arg("--target-dir")
        .arg(target_dir);
    for arg in extra_args {
        cmd.arg(arg);
    }
    let output = cmd
        .output()
        .expect("spawn cargo to build companion guest_mod");
    assert!(
        output.status.success(),
        "failed building companion guest_mod ({extra_args:?}):\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let (prefix, ext) = dylib_naming();
    let dylib = target_dir
        .join("release")
        .join(format!("{prefix}companion_guest_mod.{ext}"));
    assert!(
        dylib.is_file(),
        "expected compiled guest dylib at {}",
        dylib.display()
    );
    dylib
}

/// Compiles (incrementally via Cargo) both Mod A (`companion_core_mod`) and Mod B
/// (`companion_fx_mod`) from the standalone zero-Bevy `guest_mod/` crate once per process.
pub fn ensure_companion_guest_mods_built() -> CompanionGuestMods {
    static BUILT: OnceLock<CompanionGuestMods> = OnceLock::new();
    BUILT
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let guest_dir = manifest_dir.join("guest_mod");
            let guest_manifest = guest_dir.join("Cargo.toml");
            let mod_a_target = guest_dir.join("target").join("mod_a");
            let mod_b_target = guest_dir.join("target").join("mod_b");

            let mod_a_path = build_guest_variant(&guest_manifest, &mod_a_target, &[]);
            let mod_b_path =
                build_guest_variant(&guest_manifest, &mod_b_target, &["--features", "mod-b"]);

            CompanionGuestMods {
                mod_a_path,
                mod_b_path,
            }
        })
        .clone()
}

/// Constructs a complete Bevy `0.20.1` companion modding [`App`] combining all Phase 3 companion
/// bridges, `.wesl` sprite materials, headless `RenderApp` extraction, budget watchdogs,
/// reflection/BRP telemetry, and versioned save-game persistence.
///
/// When `headless` is `true`, the app is configured for deterministic headless stepping via
/// `app.update()` without requiring a windowing backend, GPU adapter, or OS audio device.
pub fn build_companion_app(headless: bool) -> App {
    let mut app = App::new();
    app.insert_resource(CompanionRunConfig { headless });

    let budget_policy = ModBudgetPolicy::default()
        .with_max_spawns_per_frame(Some(256))
        .with_max_arena_bytes_per_system(Some(64 * 1024))
        .with_max_p99_system_micros(Some(250_000));

    // Plugin ordering invariant:
    // 1. `AbiModPlugin` initializes `ModRegistry` and `ModBudgetTelemetry`.
    // 2. `ModBridgesPlugin` registers `StatesPlugin`, `StdTypesBridgePlugin`, and `ModAssetPlugin`
    //    BEFORE `AssetPlugin` so the isolated `"mod"` AssetSource is registered prior to
    //    `AssetServer` construction.
    app.add_plugins((
        TaskPoolPlugin::default(),
        AbiModPlugin::with_budget_policy(budget_policy),
        ModBridgesPlugin,
        AssetPlugin::default(),
        InputPlugin,
        ModCameraLightBridgePlugin,
        ModAudioBridgePlugin,
        ModPickingBridgePlugin,
        ModInputExtBridgePlugin,
        ModUiBridgePlugin,
    ));
    app.add_plugins((
        ModSubAppPlugin::<RenderApp>::default(),
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
        RemotePlugin::default(),
        ModReflectPlugin,
        ModSaveGamePlugin::new(),
        ModTelemetryPlugin::new(),
    ));

    // Explicit `RemSize` for deterministic `Val::Rem` / `FontSize::Rem` layout evaluation.
    app.insert_resource(RemSize(16.0));

    // Expose persistent host component across the C-ABI boundary.
    app.register_mod_component::<CompanionBeacon>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register CompanionBeacon");

    app
}

/// Invokes a live `bevy_remote` BRP JSON-RPC method (`bam/list_mods`, `bam/system_metrics`,
/// `bam/component_schemas`, etc.) directly against `world`.
pub fn call_brp_method(world: &mut World, method: &str, params: Option<Value>) -> BrpResult {
    let sys_id = match *world
        .resource::<RemoteMethods>()
        .get(method)
        .unwrap_or_else(|| panic!("missing RemoteMethods entry '{method}'"))
    {
        RemoteMethodSystemId::Instant(id) => id,
        RemoteMethodSystemId::Watching(_) => panic!("expected Instant BRP method '{method}'"),
    };
    world
        .run_system_with(sys_id, params)
        .expect("run_system_with BRP method")
}

/// Spawns the cohesive companion scene owned by `mod_a` (presentation, camera rig, lights,
/// spatial audio, retained UI HUD, and `.wesl` sprites marked `ModScoped(mod_a)`) plus three
/// persistent (`!ModScoped`) beacon entities saved by `ModSaveGamePlugin`.
pub fn spawn_companion_scene(app: &mut App, mod_a: ModId) -> CompanionSceneHandles {
    let assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets");
    if assets_dir.is_dir() {
        app.mount_mod_asset_dir_for_mod(mod_a, "companion_core_mod", &assets_dir)
            .expect("mount companion_core_mod asset dir");
    }

    // 1. Three persistent (`!ModScoped`) studio beacons persisted across save/load.
    let beacon_alpha = app
        .world_mut()
        .spawn((
            Name::new("beacon_alpha"),
            Transform::from_xyz(-3.0, 1.0, 0.0),
            CompanionBeacon {
                beacon_id: 1,
                press_count: 0,
                last_button: 0,
                energy: 100.0,
                active_tab_index: 0,
                ui_events: 0,
            },
        ))
        .id();
    let beacon_beta = app
        .world_mut()
        .spawn((
            Name::new("beacon_beta"),
            Transform::from_xyz(0.0, 1.5, -2.0),
            CompanionBeacon {
                beacon_id: 2,
                press_count: 0,
                last_button: 0,
                energy: 150.0,
                active_tab_index: 0,
                ui_events: 0,
            },
        ))
        .id();
    let beacon_gamma = app
        .world_mut()
        .spawn((
            Name::new("beacon_gamma"),
            Transform::from_xyz(3.0, 2.0, 1.0),
            CompanionBeacon {
                beacon_id: 3,
                press_count: 0,
                last_button: 0,
                energy: 200.0,
                active_tab_index: 0,
                ui_events: 0,
            },
        ))
        .id();

    // 2. ModScoped 3D Camera + PanOrbitCamera + DirectionalLight + PointLight.
    let camera_rig = app
        .world_mut()
        .spawn((
            FfiCamera3d {
                order: 0,
                clear_color: [0.05, 0.07, 0.12, 1.0],
                clear_color_mode: CLEAR_COLOR_CUSTOM,
                hdr: FfiBool::TRUE,
                tonemapping: TONEMAPPING_AGX,
                deband_dither: FfiBool::TRUE,
                transmission_enabled: FfiBool::TRUE,
                transmission_steps: 2,
                transmission_quality: TRANSMISSION_QUALITY_HIGH,
                ..Default::default()
            },
            FfiPanOrbitCamera {
                last_anchor_depth: -12.0,
                orbit_constraint_mode: ORBIT_CONSTRAINT_FIXED,
                orbit_up: [0.0, 1.0, 0.0],
                enabled_pan: FfiBool::TRUE,
                enabled_orbit: FfiBool::TRUE,
                enabled_zoom: FfiBool::TRUE,
                sensitivity_orbit: [1.25, 1.25],
                ..Default::default()
            },
            ModScoped(mod_a),
        ))
        .id();

    let directional_light = app
        .world_mut()
        .spawn((
            FfiDirectionalLight {
                color: [1.0, 0.96, 0.88, 1.0],
                illuminance: 32_000.0,
                shadow_maps_enabled: FfiBool::TRUE,
                contact_shadows_enabled: FfiBool::TRUE,
                ..Default::default()
            },
            ModScoped(mod_a),
        ))
        .id();

    let point_light = app
        .world_mut()
        .spawn((
            FfiPointLight {
                color: [0.35, 0.85, 1.0, 1.0],
                intensity: 2_400.0,
                range: 30.0,
                radius: 0.35,
                shadow_maps_enabled: FfiBool::TRUE,
                ..Default::default()
            },
            ModScoped(mod_a),
        ))
        .id();

    // 3. ModScoped Spatial AudioPlayer (purely in-memory AudioSource, zero OS audio device).
    let audio_type = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(AUDIO_SOURCE_ASSET_ALIAS)
        .expect("AudioSource asset type registered by ModAudioBridgePlugin")
        .handle;
    let audio_clip = app
        .world_mut()
        .resource_mut::<Assets<AudioSource>>()
        .add(AudioSource {
            bytes: Arc::from([0u8; 32]),
        });
    let ffi_audio_clip = audio_source_handle_to_ffi(audio_type, &audio_clip);
    let spatial_audio_emitter = app
        .world_mut()
        .spawn((
            Transform::from_xyz(4.0, 1.5, -3.0),
            FfiAudioPlayerSpec::new(
                ffi_audio_clip,
                FfiAudioPlaybackSettings {
                    mode: PLAYBACK_MODE_LOOP,
                    spatial: FfiBool::TRUE,
                    volume: 0.75,
                    speed: 1.0,
                    ..Default::default()
                },
            ),
            ModScoped(mod_a),
        ))
        .id();

    // 4. ModScoped Retained UI HUD (`Val::Rem` + `FontSize::Rem` + Headless `TabList` + Feathers).
    let hud_root = app
        .world_mut()
        .spawn((
            FfiUiNode {
                width: FfiVal::rem(24.0),
                height: FfiVal::rem(10.0),
                padding: FfiUiRect::all(FfiVal::rem(1.0)),
                row_gap: FfiVal::rem(0.5),
                border_radius: FfiBorderRadius::elliptical_all(FfiVal::rem(0.75), FfiVal::em(0.5)),
                display: FfiUiNode::DISPLAY_FLEX,
                position_type: FfiUiNode::POSITION_ABSOLUTE,
                flex_direction: FfiUiNode::FLEX_COLUMN,
                fixed_node: FfiBool::TRUE,
                ..Default::default()
            },
            FfiThemeContext::FLOATING,
            ModScoped(mod_a),
        ))
        .id();

    let hud_title = app
        .world_mut()
        .spawn((
            ChildOf(hud_root),
            FfiUiTextStyle {
                font_size: FfiFontSize::rem(1.25),
                color: FfiRgba::new(0.92, 0.96, 1.0, 1.0),
                ensure_text: FfiBool::TRUE,
                ..Default::default()
            },
            ModScoped(mod_a),
        ))
        .id();

    let hud_tablist = app
        .world_mut()
        .spawn((
            ChildOf(hud_root),
            FfiTabList {
                orientation: FfiTabList::ORIENTATION_HORIZONTAL,
                activation: FfiTabList::ACTIVATION_AUTOMATIC,
                self_update: FfiBool::TRUE,
                disabled: FfiBool::FALSE,
            },
            FfiSelectedTab::default(),
            ModScoped(mod_a),
        ))
        .id();

    let tab_overview = app
        .world_mut()
        .spawn((ChildOf(hud_tablist), FfiTab::default(), ModScoped(mod_a)))
        .id();
    let tab_telemetry = app
        .world_mut()
        .spawn((ChildOf(hud_tablist), FfiTab::default(), ModScoped(mod_a)))
        .id();

    // 5. Custom `.wesl` `SpriteMaterial` + `ExtendedMaterial2d` with aligned `ShaderBuffer`.
    let wesl_shader = app
        .register_mod_wesl_shader(
            mod_a,
            WeslShaderAssetSpec::from_wesl(
                "companion_pulse",
                "mods/companion_core_mod/companion_pulse.wesl",
                COMPANION_PULSE_WESL,
            ),
        )
        .expect("register .wesl shader");

    let shader_buffer = app
        .register_mod_shader_buffer(
            mod_a,
            ModShaderBufferSpec::from_f32_slice("companion_palette", 32, &[0.18, 0.72, 0.96, 1.0]),
        )
        .expect("register aligned ShaderBuffer");

    let sprite_extension = app
        .register_mod_wesl_sprite_extension(
            mod_a,
            ModWeslSpriteExtensionSpec::new("companion_pulse_sprite")
                .with_vertex_shader(wesl_shader.clone())
                .with_fragment_shader(wesl_shader.clone())
                .with_vertex_entry_point("vs_companion_pulse")
                .with_fragment_entry_point("fs_companion_pulse")
                .with_shader_buffer(shader_buffer.clone())
                .with_alpha_mode(AlphaMode2d::Blend)
                .with_depth_bias(0.25)
                .with_params(ModMaterialParams {
                    tint: [0.25, 0.85, 1.0, 1.0],
                    intensity: 2.0,
                    time_scale: 1.5,
                    ..Default::default()
                })
                .with_shader_defs([
                    ModShaderDef::flag("MOD_COMPANION_PULSE"),
                    ModShaderDef::uint("PULSE_HARMONICS", 4),
                ]),
        )
        .expect("register ModWeslSpriteExtension2d");

    let extended_material = app
        .register_mod_wesl_extended_sprite_material(
            mod_a,
            ModExtendedSpriteMaterialSpec::new("companion_extended_2d")
                .with_alpha_mode(AlphaMode2d::Blend)
                .with_color_rgba([0.4, 0.9, 1.0, 1.0])
                .with_fragment_shader(wesl_shader.clone())
                .with_shader_buffer(shader_buffer.clone())
                .with_depth_bias(0.5)
                .with_params(ModMaterialParams {
                    tint: [0.9, 0.4, 1.0, 1.0],
                    intensity: 3.0,
                    time_scale: 2.0,
                    ..Default::default()
                })
                .with_shader_defs([ModShaderDef::flag("COMPANION_EXTENDED_2D")]),
        )
        .expect("register ModExtendedSpriteMaterial2d");

    let unified_sprite = app
        .spawn_mod_sprite_with_material(
            mod_a,
            ModUnifiedSpriteSpec::new()
                .with_color_rgba([0.85, 0.95, 1.0, 1.0])
                .with_custom_size(64.0, 64.0)
                .with_alpha_mode(SpriteAlphaMode::Blend)
                .with_sync_to_render_world(true),
            sprite_extension.clone(),
        )
        .expect("spawn unified SpriteMaterial entity");

    let extended_sprite = app
        .spawn_mod_extended_mesh_sprite(
            mod_a,
            ModUnifiedSpriteSpec::new()
                .with_quad_size(48.0, 48.0)
                .with_sync_to_render_world(true),
            extended_material.clone(),
        )
        .expect("spawn ExtendedMaterial2d entity");

    CompanionSceneHandles {
        persistent_beacons: [beacon_alpha, beacon_beta, beacon_gamma],
        camera_rig,
        directional_light,
        point_light,
        spatial_audio_emitter,
        hud_root,
        hud_title,
        hud_tablist,
        tab_overview,
        tab_telemetry,
        unified_sprite,
        extended_sprite,
        wesl_shader,
        shader_buffer,
        sprite_extension,
        extended_material,
        audio_clip,
    }
}

/// Verifies headless Bevy `0.20` pipeline specialization (`MaterialExtension2d::specialize` and
/// `Material2d::specialize`) for the registered `.wesl` `SpriteMaterial` and
/// `ModExtendedSpriteMaterial2d` assets without requiring a GPU device.
pub fn verify_wesl_sprite_specialization(app: &App, handles: &CompanionSceneHandles) {
    let quad_mesh = build_quad_mesh_2d(1.0, 1.0).expect("build 2D quad mesh");
    let pipeline = Material2dPipeline {
        mesh2d_pipeline: Mesh2dPipeline {
            view_layout: BindGroupLayoutDescriptor::new("companion_headless_view", &[]),
            mesh_layout: BindGroupLayoutDescriptor::new("companion_headless_mesh", &[]),
            shader: Handle::default(),
            per_object_buffer_batch_size: None,
        },
    };
    let build_base_desc = |mesh: &Mesh| -> (
        RenderPipelineDescriptor,
        bevy_render::mesh::MeshVertexBufferLayoutRef,
        Mesh2dPipelineKey,
    ) {
        let mut layouts = MeshVertexBufferLayouts::default();
        let layout = mesh.get_mesh_vertex_buffer_layout(&mut layouts);
        let base_key = BaseMeshPipelineKey::from_primitive_topology_and_strip_index(
            mesh.primitive_topology(),
            None,
        );
        let mesh_key = Mesh2dPipelineKey::from_bits_retain(base_key.bits())
            | Mesh2dPipelineKey::from_msaa_samples(1)
            | Mesh2dPipelineKey::from_target_format(TextureFormat::Bgra8UnormSrgb);
        let descriptor = pipeline
            .mesh2d_pipeline
            .specialize(mesh_key, &layout)
            .expect("specialize base Mesh2dPipeline headlessly");
        (descriptor, layout, mesh_key)
    };

    // 1. Specialize `ModWeslSpriteExtension2d` (unified `SpriteMaterial`).
    let ext = app
        .world()
        .resource::<Assets<ModWeslSpriteExtension2d>>()
        .get(&handles.sprite_extension)
        .expect("ModWeslSpriteExtension2d stored");
    let ext_key = ext.bind_group_data();
    let (mut desc, layout, mesh_key) = build_base_desc(&quad_mesh);
    <ModWeslSpriteExtension2d as MaterialExtension2d>::specialize(
        &pipeline,
        &mut desc,
        &layout,
        Material2dKey {
            mesh_key,
            bind_group_data: ext_key,
        },
    )
    .expect("specialize ModWeslSpriteExtension2d");
    assert_eq!(desc.vertex.shader, handles.wesl_shader);
    assert_eq!(
        desc.vertex.entry_point.as_deref(),
        Some("vs_companion_pulse")
    );
    let frag = desc.fragment.as_ref().expect("fragment state present");
    assert_eq!(frag.shader, handles.wesl_shader);
    assert_eq!(frag.entry_point.as_deref(), Some("fs_companion_pulse"));
    assert!(frag.shader_defs.contains(&ShaderDefVal::Bool(
        Cow::Borrowed("MOD_COMPANION_PULSE"),
        true
    )));

    // 2. Specialize `ModExtendedSpriteMaterial2d` (`ExtendedMaterial2d`).
    let ext2d = app
        .world()
        .resource::<Assets<ModExtendedSpriteMaterial2d>>()
        .get(&handles.extended_material)
        .expect("ModExtendedSpriteMaterial2d stored");
    let combined_key = ext2d.bind_group_data();
    let (mut desc2, layout2, mesh_key2) = build_base_desc(&quad_mesh);
    <ModExtendedSpriteMaterial2d as Material2d>::specialize(
        &pipeline,
        &mut desc2,
        &layout2,
        Material2dKey {
            mesh_key: mesh_key2,
            bind_group_data: combined_key,
        },
    )
    .expect("specialize ModExtendedSpriteMaterial2d");
    let frag2 = desc2.fragment.as_ref().expect("extended fragment state");
    assert_eq!(frag2.shader, handles.wesl_shader);
    assert!(frag2.shader_defs.contains(&ShaderDefVal::Bool(
        Cow::Borrowed("COMPANION_EXTENDED_2D"),
        true
    )));
}

/// Decodes a 16-byte `companion_core_mod::OrbitalBeacon` POD payload `(f32, f32, u32, u32)` safely
/// without `unsafe`.
pub fn decode_orbital_beacon_bytes(bytes: &[u8]) -> (f32, f32, u32, u32) {
    assert_eq!(
        bytes.len(),
        16,
        "companion_core_mod::OrbitalBeacon must be 16 bytes"
    );
    let orbit_phase = f32::from_ne_bytes(bytes[0..4].try_into().expect("orbit_phase bytes"));
    let harmonic_gain = f32::from_ne_bytes(bytes[4..8].try_into().expect("harmonic_gain bytes"));
    let ticks = u32::from_ne_bytes(bytes[8..12].try_into().expect("ticks bytes"));
    let pick_boosts = u32::from_ne_bytes(bytes[12..16].try_into().expect("pick_boosts bytes"));
    (orbit_phase, harmonic_gain, ticks, pick_boosts)
}

/// Executes the complete end-to-end 7-subsystem companion showcase headlessly, asserting every
/// Phase 3 capability and returning a structured [`CompanionDemoReport`].
pub fn run_companion_demo(headless: bool) -> CompanionDemoReport {
    let mods = ensure_companion_guest_mods_built();
    let mut app = build_companion_app(headless);

    let load_opts = LoadOptions::new().with_shadow_copy(true);
    let mod_a = app
        .load_mod_with(&mods.mod_a_path, load_opts.clone())
        .expect("load Mod A (companion_core_mod)");
    let mod_b = app
        .load_mod_with(&mods.mod_b_path, load_opts.clone())
        .expect("load Mod B (companion_fx_mod)");

    let handles = spawn_companion_scene(&mut app, mod_a);

    // Finalize plugins (installs `ModTelemetryPlugin` `bam/*` BRP methods and binds `AssetServer`
    // in `ModAssetPlugin::finish`) prior to headless frame stepping.
    app.finish();
    app.cleanup();

    // Frame 1: materialize bridged Bevy 0.20 components, attach Mod A & Mod B dynamic components,
    // and install runtime guest `cmd_observe` observers on `PointerPress` and `FfiValueChange`.
    app.update();

    // Frame 2: tick Mod A & Mod B systems and let TabPlugin derived state settle.
    app.update();

    // Verify Capability 1: PanOrbitCamera + 3D Camera & Lights.
    {
        let world = app.world();
        assert!(world.get::<Camera3d>(handles.camera_rig).is_some());
        assert!(world.get::<Camera>(handles.camera_rig).is_some());
        assert!(world.get::<Hdr>(handles.camera_rig).is_some());
        assert_eq!(
            world.get::<Tonemapping>(handles.camera_rig),
            Some(&Tonemapping::AgX)
        );
        assert_eq!(
            world.get::<DebandDither>(handles.camera_rig),
            Some(&DebandDither::Enabled)
        );
        assert!(
            world
                .get::<ScreenSpaceTransmission>(handles.camera_rig)
                .is_some()
        );
        let pan_orbit = world
            .get::<PanOrbitCamera>(handles.camera_rig)
            .expect("PanOrbitCamera materialized");
        assert!((pan_orbit.last_anchor_depth - (-12.0)).abs() < 1e-6);
        assert!(
            world
                .get::<DirectionalLight>(handles.directional_light)
                .is_some()
        );
        let pl = world
            .get::<PointLight>(handles.point_light)
            .expect("PointLight materialized");
        assert!(
            pl.intensity > 2_400.0,
            "Mod A must have pulsed PointLight intensity across C-ABI, got {}",
            pl.intensity
        );
    }

    // Verify Capability 2: Custom `.wesl` SpriteMaterial + ExtendedMaterial2d + RenderApp extraction.
    {
        let shaders = app.world().resource::<Assets<Shader>>();
        let stored_shader = shaders
            .get(&handles.wesl_shader)
            .expect("WESL shader asset");
        assert!(matches!(&stored_shader.source, Source::Wesl(_)));
        assert!(
            app.world()
                .resource::<Assets<ShaderBuffer>>()
                .get(&handles.shader_buffer)
                .is_some()
        );

        let unified = app.world().entity(handles.unified_sprite);
        assert!(unified.contains::<Sprite>());
        assert!(unified.contains::<Mesh2d>());
        assert!(unified.contains::<SpriteMaterial<ModWeslSpriteExtension2d>>());
        assert!(
            !unified.contains::<MeshMaterial2d<SpriteMeshMaterial>>(),
            "SpriteMaterialPlugin must suppress default SpriteMeshMaterial"
        );

        let needs_spec = app
            .world()
            .resource::<EntitiesNeedingSpecialization<ModExtendedSpriteMaterial2d>>();
        assert!(needs_spec.changed.contains(&handles.extended_sprite));

        let render_world = app
            .get_sub_app(RenderApp)
            .expect("RenderApp present")
            .world();
        assert!(
            render_world
                .resource::<DirtySpecializations>()
                .changed_renderables
                .contains(&MainEntity::from(handles.extended_sprite))
        );
    }
    verify_wesl_sprite_specialization(&app, &handles);

    // Verify Capability 4: Spatial AudioPlayer + live parameter update.
    {
        let player = app
            .world()
            .get::<AudioPlayer<AudioSource>>(handles.spatial_audio_emitter)
            .expect("AudioPlayer<AudioSource> materialized");
        assert_eq!(player.0.id(), handles.audio_clip.id());
        let ps = app
            .world()
            .get::<PlaybackSettings>(handles.spatial_audio_emitter)
            .expect("PlaybackSettings materialized");
        assert!(ps.spatial);
        assert!(matches!(ps.mode, PlaybackMode::Loop));
        assert_eq!(ps.volume, Volume::Linear(0.75));
    }
    {
        let mut spec = app
            .world_mut()
            .get_mut::<FfiAudioPlayerSpec>(handles.spatial_audio_emitter)
            .expect("FfiAudioPlayerSpec present");
        spec.settings.mode = PLAYBACK_MODE_ONCE;
        spec.settings.volume = 0.5;
    }

    // Verify Capability 5 (Layout & Theme) + trigger synthetic Capability 3 (`PointerPress`)
    // and Capability 5 (`ValueChange` on `TabList`) events!
    {
        let world = app.world();
        let node = world
            .get::<Node>(handles.hud_root)
            .expect("HUD Node synced");
        assert_eq!(node.width, Val::Rem(24.0));
        assert!(world.get::<FixedNode>(handles.hud_root).is_some());
        assert_eq!(
            world.get::<ThemeContext>(handles.hud_root),
            Some(&ThemeContext(SurfaceLevel::Floating))
        );
        let tf = world
            .get::<TextFont>(handles.hud_title)
            .expect("HUD TextFont synced");
        assert_eq!(tf.font_size, FontSize::Rem(1.25));
        assert_eq!(
            tf.font_size
                .eval(Vec2::new(1280.0, 720.0), *world.resource::<RemSize>()),
            20.0
        );
        assert!(world.get::<TabList>(handles.hud_tablist).is_some());
        assert!(world.get::<Tab>(handles.tab_overview).is_some());
        assert!(world.get::<Tab>(handles.tab_telemetry).is_some());
    }

    let primary_beacon_entity = handles.persistent_beacons[0];
    let pointer = Pointer::new(
        PointerId::Mouse,
        Location {
            target: NormalizedRenderTarget::None {
                width: 1280,
                height: 720,
            },
            position: Vec2::new(640.0, 360.0),
        },
    );
    let hit = HitData::new(
        handles.camera_rig,
        4.5,
        Some(Vec3::new(-3.0, 1.0, 0.0)),
        Some(Vec3::Y),
    );
    app.world_mut().trigger(PointerPress {
        entity: primary_beacon_entity,
        pointer,
        button: PointerButton::Primary,
        hit,
        count: 2,
    });
    app.world_mut().trigger(ValueChange::<Option<Entity>> {
        source: handles.hud_tablist,
        value: Some(handles.tab_telemetry),
        is_final: true,
    });

    // Frame 3: flush observer commands, apply `pick_boosts`, sync updated audio settings & TabList.
    app.update();

    let primary_beacon_after_events = *app
        .world()
        .get::<CompanionBeacon>(primary_beacon_entity)
        .expect("primary CompanionBeacon");
    assert_eq!(primary_beacon_after_events.press_count, 2);
    assert_eq!(
        primary_beacon_after_events.last_button,
        u32::from(FfiPointerButton::PRIMARY_RAW)
    );
    assert!(
        primary_beacon_after_events.energy >= 129.5,
        "PointerPress guest cmd_observe must boost beacon energy, got {}",
        primary_beacon_after_events.energy
    );
    assert_eq!(primary_beacon_after_events.ui_events, 1);
    assert_eq!(primary_beacon_after_events.active_tab_index, 1);

    // Verify TabList selection state & audio settings update.
    assert_eq!(
        app.world().get::<SelectedTab>(handles.hud_tablist),
        Some(&SelectedTab(Some(handles.tab_telemetry)))
    );
    assert_eq!(
        app.world().get::<FfiSelectedTab>(handles.hud_tablist),
        Some(&FfiSelectedTab::from_option(Some(handles.tab_telemetry)))
    );
    assert!(app.world().get::<Selected>(handles.tab_telemetry).is_some());
    assert!(app.world().get::<Selected>(handles.tab_overview).is_none());
    let updated_ps = app
        .world()
        .get::<PlaybackSettings>(handles.spatial_audio_emitter)
        .expect("updated PlaybackSettings");
    assert!(matches!(updated_ps.mode, PlaybackMode::Once));
    assert_eq!(updated_ps.volume, Volume::Linear(0.5));

    let synced_point_light_intensity = app
        .world()
        .get::<PointLight>(handles.point_light)
        .expect("synced PointLight")
        .intensity;
    assert!(synced_point_light_intensity >= 2_775.0);

    // Verify Capability 6: ModBudgetPolicy + ModTelemetryPlugin live metrics & BRP queries.
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0,
        "demo must run with zero guest system faults"
    );
    assert!(
        app.world().contains_resource::<DiagnosticsStore>(),
        "diagnostics feature must populate DiagnosticsStore"
    );

    let budget_snapshots = app.system_budget_snapshots();
    assert!(
        budget_snapshots.len() >= 5,
        "expected >= 5 tracked guest systems across Mod A + Mod B, got {}",
        budget_snapshots.len()
    );
    assert!(budget_snapshots.iter().all(|s| !s.quarantined));

    let list_mods_json =
        call_brp_method(app.world_mut(), BRP_METHOD_BAM_LIST_MODS, None).expect("bam/list_mods");
    let brp_total_mods = list_mods_json["total_mods"].as_u64().unwrap_or(0);
    assert_eq!(brp_total_mods, 2);

    let metrics_json = call_brp_method(app.world_mut(), BRP_METHOD_BAM_SYSTEM_METRICS, None)
        .expect("bam/system_metrics");
    let metrics_arr = metrics_json["systems"]
        .as_array()
        .expect("bam/system_metrics systems array");
    assert!(
        metrics_arr.len() >= 5,
        "expected >= 5 systems in bam/system_metrics, got {}",
        metrics_arr.len()
    );
    let mut system_metrics_us = Vec::with_capacity(metrics_arr.len());
    for entry in metrics_arr {
        let mod_name = entry["mod_name"].as_str().unwrap_or("unknown");
        let sys_name = entry["system_name"].as_str().unwrap_or("unknown");
        let p50_us = entry["p50_us"].as_u64().expect("p50_us u64");
        let p99_us = entry["p99_us"].as_u64().expect("p99_us u64");
        assert!(p99_us >= p50_us, "p99_us must be >= p50_us");
        assert_eq!(entry["quarantined"], false);
        system_metrics_us.push((format!("{mod_name}::{sys_name}"), p50_us, p99_us));
    }

    let schemas_json = call_brp_method(
        app.world_mut(),
        BRP_METHOD_BAM_COMPONENT_SCHEMAS,
        Some(serde_json::json!({ "component": MOD_A_ORBITAL_BEACON_NAME })),
    )
    .expect("bam/component_schemas");
    let schema_comps = schemas_json["components"]
        .as_array()
        .expect("components schema array");
    assert_eq!(schema_comps.len(), 1);
    assert_eq!(
        schema_comps[0]["fields"].as_array().map(Vec::len),
        Some(4),
        "OrbitalBeacon must expose 4 declarative BamFieldDesc fields"
    );

    // Snapshot pre-save Host + Mod A bytes for exact byte-for-byte recovery verification.
    let orbital_handle_save = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(MOD_A_ORBITAL_BEACON_NAME)
        .expect("OrbitalBeacon registered")
        .handle;
    let mut expected_saved: BTreeMap<u32, (CompanionBeacon, Vec<u8>)> = BTreeMap::new();
    for &entity in &handles.persistent_beacons {
        let beacon = *app
            .world()
            .get::<CompanionBeacon>(entity)
            .expect("saved CompanionBeacon");
        let orbital_bytes = read_entity_pod_bytes(app.world(), entity, orbital_handle_save)
            .expect("read OrbitalBeacon POD bytes");
        let (_phase, _gain, ticks, pick_boosts) = decode_orbital_beacon_bytes(&orbital_bytes);
        assert!(ticks >= 2);
        if beacon.beacon_id == 1 {
            assert_eq!(pick_boosts, 1, "primary beacon must record 1 pick boost");
        }
        expected_saved.insert(beacon.beacon_id, (beacon, orbital_bytes));
    }

    // Verify Capability 7: ModSaveGamePlugin Quick-Save -> Mod-Disable -> Quick-Load Recovery!
    let archive = app.save_mod_archive().expect("save_mod_archive");
    assert_eq!(
        archive.entities.len(),
        3,
        "only the 3 persistent beacons must be saved (all ModScoped entities excluded)"
    );
    assert_eq!(archive.mods.len(), 2);

    // Verify both JSON and binary archive framing roundtrip identically.
    let json_text = archive.to_json_string().expect("archive JSON encode");
    assert_eq!(
        ModSaveArchive::from_json_str(&json_text).expect("archive JSON decode"),
        archive
    );
    let bin_bytes = archive.to_binary_bytes().expect("archive binary encode");
    let decoded_archive =
        ModSaveArchive::from_binary_bytes(&bin_bytes).expect("archive binary decode");
    assert_eq!(decoded_archive, archive);

    // Unload Mod B on the live world to verify clean live teardown while Mod A survives.
    app.unload_mod(mod_b).expect("unload Mod B cleanly");
    app.update();
    assert_eq!(app.world().resource::<Messages<ModUnloaded>>().len(), 1);
    assert_eq!(
        app.world().resource::<Messages<ModSystemFaulted>>().len(),
        0
    );

    // Build a recovery world with ONLY Host + Mod A loaded (Mod B disabled!).
    let mut recovery_app = build_companion_app(true);
    let _mod_a_recovery = recovery_app
        .load_mod_with(&mods.mod_a_path, load_opts)
        .expect("load Mod A in recovery app");
    recovery_app.finish();
    recovery_app.cleanup();

    // Step 7a: StrictRequireMods MUST reject atomically with MissingMods { ["companion_fx_mod"] }.
    let strict_err = recovery_app
        .load_mod_archive(&decoded_archive, ModSaveLoadPolicy::StrictRequireMods)
        .expect_err("StrictRequireMods must fail when Mod B is disabled");
    match strict_err {
        ModSaveLoadError::MissingMods { missing } => {
            assert_eq!(missing, vec!["companion_fx_mod".to_owned()]);
        }
        other => panic!("expected MissingMods for companion_fx_mod, got {other:?}"),
    }
    assert_eq!(
        recovery_app
            .world_mut()
            .query::<&CompanionBeacon>()
            .iter(recovery_app.world())
            .count(),
        0,
        "failed StrictRequireMods load must not leak partial entities"
    );

    // Step 7b: StripMissingModComponents recovers cleanly, stripping Mod B while preserving
    // Host + Mod A components byte-for-byte!
    let recovery_report = recovery_app
        .load_mod_archive(
            &decoded_archive,
            ModSaveLoadPolicy::StripMissingModComponents,
        )
        .expect("StripMissingModComponents quick-load recovery");
    assert_eq!(recovery_report.restored_entities, 3);
    assert_eq!(recovery_report.stripped_components, 3);
    assert_eq!(
        recovery_report.stripped_component_names,
        vec![MOD_B_PLASMA_AURA_NAME.to_owned()]
    );

    let orbital_handle_recovery = recovery_app
        .world()
        .resource::<ModRegistry>()
        .get_by_name(MOD_A_ORBITAL_BEACON_NAME)
        .expect("OrbitalBeacon registered in recovery app")
        .handle;
    assert!(
        recovery_app
            .world()
            .resource::<ModRegistry>()
            .get_by_name(MOD_B_PLASMA_AURA_NAME)
            .is_none(),
        "disabled Mod B component must not exist in recovery world"
    );

    let mut restored_entities: BTreeMap<u32, Entity> = BTreeMap::new();
    for (&_old_bits, &new_entity) in &recovery_report.entity_map {
        let beacon = *recovery_app
            .world()
            .get::<CompanionBeacon>(new_entity)
            .expect("restored Host CompanionBeacon");
        let (expected_beacon, expected_orbital_bytes) = expected_saved
            .get(&beacon.beacon_id)
            .expect("matching beacon_id");
        assert_eq!(&beacon, expected_beacon);

        let restored_orbital_bytes =
            read_entity_pod_bytes(recovery_app.world(), new_entity, orbital_handle_recovery)
                .expect("read restored OrbitalBeacon POD bytes");
        assert_eq!(
            &restored_orbital_bytes, expected_orbital_bytes,
            "Mod A OrbitalBeacon bytes must match byte-for-byte after StripMissingModComponents"
        );
        restored_entities.insert(beacon.beacon_id, new_entity);
    }
    assert_eq!(restored_entities.len(), 3);

    // Resume live simulation in the recovered world for 2 frames -> Mod A keeps ticking cleanly!
    recovery_app.update();
    recovery_app.update();
    assert_eq!(
        recovery_app
            .world()
            .resource::<Messages<ModSystemFaulted>>()
            .len(),
        0
    );
    let recovered_alpha = restored_entities[&1];
    let resumed_orbital_bytes = read_entity_pod_bytes(
        recovery_app.world(),
        recovered_alpha,
        orbital_handle_recovery,
    )
    .expect("read resumed OrbitalBeacon bytes");
    let (_, _, post_recovery_orbital_ticks, _) =
        decode_orbital_beacon_bytes(&resumed_orbital_bytes);
    let (_, _, pre_save_ticks, _) = decode_orbital_beacon_bytes(&expected_saved[&1].1);
    assert_eq!(
        post_recovery_orbital_ticks,
        pre_save_ticks + 2,
        "Mod A must seamlessly resume ticking restored persistent entities"
    );

    CompanionDemoReport {
        headless,
        synced_point_light_intensity,
        primary_beacon_after_events,
        brp_total_mods,
        brp_tracked_systems: system_metrics_us.len(),
        system_metrics_us,
        saved_persistent_entities: archive.entities.len(),
        recovered_entities: recovery_report.restored_entities,
        stripped_mod_b_components: recovery_report.stripped_components,
        stripped_component_names: recovery_report.stripped_component_names,
        post_recovery_orbital_ticks,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_sys::assert_abi_pod;

    #[test]
    fn companion_beacon_is_zero_padding_abi_pod() {
        assert_abi_pod::<CompanionBeacon>();
        assert_eq!(core::mem::size_of::<CompanionBeacon>(), 24);
        assert_eq!(core::mem::align_of::<CompanionBeacon>(), 4);
    }

    #[test]
    fn build_companion_app_installs_all_phase3_bridges_headlessly() {
        let app = build_companion_app(true);
        assert_eq!(
            app.world().resource::<CompanionRunConfig>(),
            &CompanionRunConfig { headless: true }
        );
        let reg = app.world().resource::<ModRegistry>();
        for canonical in [
            "companion::Beacon",
            "bevy_abi_mod::camera_light::FfiCamera3d",
            "bevy_abi_mod::camera_light::FfiPanOrbitCamera",
            "bevy_abi_mod::camera_light::FfiPointLight",
            "bevy_abi_mod::camera_light::FfiDirectionalLight",
            "bevy_picking::events::PointerPress",
            "bevy_abi_mod_ui::FfiUiNode",
            "bevy_abi_mod_ui::FfiTabList",
            "bevy_abi_mod_ui::FfiValueChange",
            "bevy_abi_mod_ui::FfiThemeContext",
            AUDIO_SOURCE_ASSET_ALIAS,
        ] {
            assert!(
                reg.get_by_name(canonical).is_some(),
                "missing expected ModRegistry entry '{canonical}'"
            );
        }
    }
}
