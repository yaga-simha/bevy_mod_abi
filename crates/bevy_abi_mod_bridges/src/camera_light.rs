//! Bevy `0.20` 2D/3D Camera, Lighting, `PanOrbitCamera`, and `SolariLighting` FFI bridges
//! (`ModCameraLightBridgePlugin`).
//!
//! Exposes zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror components and
//! bidirectional Bevy ECS sync systems under `#![forbid(unsafe_code)]`:
//!
//! - `feature = "bridge-light"`: `FfiCamera2d` (`Camera2d` + `Camera` + `Hdr`), `FfiCamera3d`
//!   (`Camera3d` + `Camera` + `Tonemapping` + `DebandDither` + `Exposure` + `Hdr` + optional
//!   `ScreenSpaceTransmission`), `FfiPointLight` (`PointLight`), `FfiSpotLight` (`SpotLight`),
//!   and `FfiDirectionalLight` (`DirectionalLight`).
//! - `feature = "bridge-camera-controller"`: `FfiPanOrbitCamera` mirroring Bevy `0.20`'s
//!   upstreamed `PanOrbitCamera`.
//! - `feature = "bridge-solari"`: `FfiSolariLighting` mirroring `bevy_solari::realtime::SolariLighting`.
//!
//! Because synced Bevy components live directly on the mod's spawned entity carrying
//! `ModScoped(mod_id)`, unloading a mod automatically despawns its cameras, lights, controllers,
//! and Solari lighting components without leaking host state.

use bevy_abi_mod_host::ModWorldExt;
use bevy_abi_mod_host_sys::{ComponentCollisionError, ComponentOptions, ModRegistry};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{FfiBool, assert_abi_pod};
use bevy_app::{App, Plugin, PostUpdate};
use bevy_ecs::change_detection::{DetectChanges, DetectChangesMut};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::query::{Changed, Without};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::system::{Commands, Query};
use bevy_ecs::world::{Ref, World};

#[cfg(feature = "bridge-light")]
use bevy_camera::{
    Camera, Camera2d, Camera3d, Camera3dDepthLoadOp, Camera3dDepthTextureUsage, ClearColorConfig,
    Exposure, Hdr,
};
#[cfg(feature = "bridge-camera-controller")]
use bevy_camera_controller::pan_orbit_camera::controller::{
    component::{EnabledMotion, OrbitConstraint, PanOrbitCamera, Sensitivity},
    momentum::Momentum,
    projections::{OrthographicSettings, PerspectiveSettings},
    smoothing::Smoothing,
    zoom::ZoomLimits,
};
#[cfg(feature = "bridge-light")]
use bevy_color::{Color, LinearRgba};
#[cfg(feature = "bridge-light")]
use bevy_ecs::query::{Has, With};
#[cfg(feature = "bridge-light")]
use bevy_light::{DirectionalLight, PointLight, SpotLight};
#[cfg(feature = "bridge-camera-controller")]
use bevy_math::{DVec3, Vec2};
#[cfg(feature = "bridge-light")]
use bevy_pbr::{ScreenSpaceTransmission, ScreenSpaceTransmissionQuality};
#[cfg(feature = "bridge-light")]
use bevy_render::view::{DebandDither, Tonemapping};
#[cfg(feature = "bridge-solari")]
use bevy_solari::realtime::SolariLighting;
#[cfg(feature = "bridge-camera-controller")]
use core::time::Duration;

// ---------------------------------------------------------------------------
// Canonical component names & wire discriminants
// ---------------------------------------------------------------------------

/// Canonical ABI name for [`FfiCamera2d`].
#[cfg(feature = "bridge-light")]
pub const CANONICAL_CAMERA_2D_NAME: &str = "bevy_abi_mod::camera_light::FfiCamera2d";
/// Canonical ABI name for [`FfiCamera3d`].
#[cfg(feature = "bridge-light")]
pub const CANONICAL_CAMERA_3D_NAME: &str = "bevy_abi_mod::camera_light::FfiCamera3d";
/// Canonical ABI name for [`FfiPointLight`].
#[cfg(feature = "bridge-light")]
pub const CANONICAL_POINT_LIGHT_NAME: &str = "bevy_abi_mod::camera_light::FfiPointLight";
/// Canonical ABI name for [`FfiSpotLight`].
#[cfg(feature = "bridge-light")]
pub const CANONICAL_SPOT_LIGHT_NAME: &str = "bevy_abi_mod::camera_light::FfiSpotLight";
/// Canonical ABI name for [`FfiDirectionalLight`].
#[cfg(feature = "bridge-light")]
pub const CANONICAL_DIRECTIONAL_LIGHT_NAME: &str =
    "bevy_abi_mod::camera_light::FfiDirectionalLight";
/// Canonical ABI name for [`FfiPanOrbitCamera`].
#[cfg(feature = "bridge-camera-controller")]
pub const CANONICAL_PAN_ORBIT_CAMERA_NAME: &str = "bevy_abi_mod::camera_light::FfiPanOrbitCamera";
/// Canonical ABI name for [`FfiSolariLighting`].
#[cfg(feature = "bridge-solari")]
pub const CANONICAL_SOLARI_LIGHTING_NAME: &str = "bevy_abi_mod::camera_light::FfiSolariLighting";

/// Clear color mode: inherit the world's `ClearColor` resource (`ClearColorConfig::Default`).
#[cfg(feature = "bridge-light")]
pub const CLEAR_COLOR_DEFAULT: u8 = 0;
/// Clear color mode: clear with custom linear RGBA (`ClearColorConfig::Custom`).
#[cfg(feature = "bridge-light")]
pub const CLEAR_COLOR_CUSTOM: u8 = 1;
/// Clear color mode: do not clear before rendering (`ClearColorConfig::None`).
#[cfg(feature = "bridge-light")]
pub const CLEAR_COLOR_NONE: u8 = 2;

/// 3D camera depth load op: clear depth buffer (`Camera3dDepthLoadOp::Clear`).
#[cfg(feature = "bridge-light")]
pub const DEPTH_LOAD_OP_CLEAR: u8 = 0;
/// 3D camera depth load op: load existing depth buffer (`Camera3dDepthLoadOp::Load`).
#[cfg(feature = "bridge-light")]
pub const DEPTH_LOAD_OP_LOAD: u8 = 1;

/// `Tonemapping::None` discriminant (`0`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_NONE: u8 = 0;
/// `Tonemapping::Linear` discriminant (`1`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_LINEAR: u8 = 1;
/// `Tonemapping::Reinhard` discriminant (`2`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_REINHARD: u8 = 2;
/// `Tonemapping::ReinhardLuminance` discriminant (`3`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_REINHARD_LUMINANCE: u8 = 3;
/// `Tonemapping::AcesFitted` discriminant (`4`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_ACES_FITTED: u8 = 4;
/// `Tonemapping::AgX` discriminant (`5`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_AGX: u8 = 5;
/// `Tonemapping::SomewhatBoringDisplayTransform` discriminant (`6`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_SOMEWHAT_BORING_DISPLAY_TRANSFORM: u8 = 6;
/// `Tonemapping::TonyMcMapface` discriminant (`7`, Bevy default).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_TONY_MC_MAPFACE: u8 = 7;
/// `Tonemapping::BlenderFilmic` discriminant (`8`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_BLENDER_FILMIC: u8 = 8;
/// `Tonemapping::KhronosPbrNeutral` discriminant (`9`).
#[cfg(feature = "bridge-light")]
pub const TONEMAPPING_KHRONOS_PBR_NEUTRAL: u8 = 9;

/// `ScreenSpaceTransmissionQuality::Low` discriminant (`0`).
#[cfg(feature = "bridge-light")]
pub const TRANSMISSION_QUALITY_LOW: u8 = 0;
/// `ScreenSpaceTransmissionQuality::Medium` discriminant (`1`, Bevy default).
#[cfg(feature = "bridge-light")]
pub const TRANSMISSION_QUALITY_MEDIUM: u8 = 1;
/// `ScreenSpaceTransmissionQuality::High` discriminant (`2`).
#[cfg(feature = "bridge-light")]
pub const TRANSMISSION_QUALITY_HIGH: u8 = 2;
/// `ScreenSpaceTransmissionQuality::Ultra` discriminant (`3`).
#[cfg(feature = "bridge-light")]
pub const TRANSMISSION_QUALITY_ULTRA: u8 = 3;

/// `OrbitConstraint::Fixed` discriminant (`0`, Bevy default).
#[cfg(feature = "bridge-camera-controller")]
pub const ORBIT_CONSTRAINT_FIXED: u8 = 0;
/// `OrbitConstraint::Free` discriminant (`1`).
#[cfg(feature = "bridge-camera-controller")]
pub const ORBIT_CONSTRAINT_FREE: u8 = 1;

// ---------------------------------------------------------------------------
// SystemSet & Plugin
// ---------------------------------------------------------------------------

/// Ordered [`SystemSet`] phases in [`PostUpdate`] for bidirectional camera and lighting sync.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CameraLightBridgeSystems {
    /// Spawns missing Bevy camera/light components and pushes changed `Ffi*` mirror fields
    /// into their paired Bevy components.
    Push,
    /// Pulls host-side Bevy component mutations back into paired `Ffi*` mirror components.
    Pull,
}

/// Bevy [`Plugin`] registering all enabled camera, lighting, `PanOrbitCamera`, and
/// `SolariLighting` FFI mirror components in [`ModRegistry`] and installing bidirectional
/// [`PostUpdate`] sync systems (`#![forbid(unsafe_code)]`).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModCameraLightBridgePlugin;

impl Plugin for ModCameraLightBridgePlugin {
    fn build(&self, app: &mut App) {
        register_camera_light_bridges(app.world_mut())
            .expect("ModCameraLightBridgePlugin registration collided in ModRegistry");

        app.configure_sets(
            PostUpdate,
            CameraLightBridgeSystems::Push.before(CameraLightBridgeSystems::Pull),
        );

        #[cfg(feature = "bridge-light")]
        app.add_systems(
            PostUpdate,
            (
                (
                    spawn_camera_2d_from_ffi,
                    push_camera_2d_to_bevy,
                    spawn_camera_3d_from_ffi,
                    push_camera_3d_to_bevy,
                    spawn_point_light_from_ffi,
                    push_point_light_to_bevy,
                    spawn_spot_light_from_ffi,
                    push_spot_light_to_bevy,
                    spawn_directional_light_from_ffi,
                    push_directional_light_to_bevy,
                )
                    .in_set(CameraLightBridgeSystems::Push),
                (
                    pull_camera_2d_from_bevy,
                    pull_camera_3d_from_bevy,
                    pull_point_light_from_bevy,
                    pull_spot_light_from_bevy,
                    pull_directional_light_from_bevy,
                )
                    .in_set(CameraLightBridgeSystems::Pull),
            ),
        );

        #[cfg(feature = "bridge-camera-controller")]
        app.add_systems(
            PostUpdate,
            (
                (
                    spawn_pan_orbit_camera_from_ffi,
                    push_pan_orbit_camera_to_bevy,
                )
                    .in_set(CameraLightBridgeSystems::Push),
                pull_pan_orbit_camera_from_bevy.in_set(CameraLightBridgeSystems::Pull),
            ),
        );

        #[cfg(feature = "bridge-solari")]
        app.add_systems(
            PostUpdate,
            (
                (spawn_solari_lighting_from_ffi, push_solari_lighting_to_bevy)
                    .in_set(CameraLightBridgeSystems::Push),
                pull_solari_lighting_from_bevy.in_set(CameraLightBridgeSystems::Pull),
            ),
        );
    }
}

/// Registers all enabled camera, light, camera-controller, and Solari mirror `ModComponent`s
/// into `world`'s [`ModRegistry`] idempotently.
pub fn register_camera_light_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }

    #[cfg(feature = "bridge-light")]
    {
        register_mirror_if_missing::<FfiCamera2d>(world)?;
        register_mirror_if_missing::<FfiCamera3d>(world)?;
        register_mirror_if_missing::<FfiPointLight>(world)?;
        register_mirror_if_missing::<FfiSpotLight>(world)?;
        register_mirror_if_missing::<FfiDirectionalLight>(world)?;
    }

    #[cfg(feature = "bridge-camera-controller")]
    {
        register_mirror_if_missing::<FfiPanOrbitCamera>(world)?;
    }

    #[cfg(feature = "bridge-solari")]
    {
        register_mirror_if_missing::<FfiSolariLighting>(world)?;
    }

    Ok(())
}

fn register_mirror_if_missing<T>(world: &mut World) -> Result<(), ComponentCollisionError>
where
    T: Component + bevy_abi_mod_sys::ModComponent + bevy_abi_mod_sys::RawInsertable + Default,
{
    if world
        .resource::<ModRegistry>()
        .get_by_name(T::CANONICAL_NAME)
        .is_none()
    {
        world.register_mod_component::<T>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Conversion helpers (`bridge-light`)
// ---------------------------------------------------------------------------

#[cfg(feature = "bridge-light")]
#[inline]
fn color_to_linear_rgba(color: Color) -> [f32; 4] {
    let linear = color.to_linear();
    [linear.red, linear.green, linear.blue, linear.alpha]
}

#[cfg(feature = "bridge-light")]
#[inline]
fn linear_rgba_to_color(rgba: [f32; 4]) -> Option<Color> {
    if rgba.iter().all(|v| v.is_finite()) {
        Some(Color::LinearRgba(LinearRgba {
            red: rgba[0],
            green: rgba[1],
            blue: rgba[2],
            alpha: rgba[3],
        }))
    } else {
        None
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn clear_color_to_ffi(config: ClearColorConfig) -> (u8, [f32; 4]) {
    match config {
        ClearColorConfig::Default => (CLEAR_COLOR_DEFAULT, [0.0, 0.0, 0.0, 1.0]),
        ClearColorConfig::Custom(color) => (CLEAR_COLOR_CUSTOM, color_to_linear_rgba(color)),
        ClearColorConfig::None => (CLEAR_COLOR_NONE, [0.0, 0.0, 0.0, 0.0]),
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn ffi_to_clear_color(mode: u8, rgba: [f32; 4]) -> ClearColorConfig {
    match mode {
        CLEAR_COLOR_CUSTOM => {
            ClearColorConfig::Custom(linear_rgba_to_color(rgba).unwrap_or(Color::BLACK))
        }
        CLEAR_COLOR_NONE => ClearColorConfig::None,
        _ => ClearColorConfig::Default,
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn clear_color_eq(a: ClearColorConfig, b: ClearColorConfig) -> bool {
    match (a, b) {
        (ClearColorConfig::Default, ClearColorConfig::Default)
        | (ClearColorConfig::None, ClearColorConfig::None) => true,
        (ClearColorConfig::Custom(ca), ClearColorConfig::Custom(cb)) => {
            color_to_linear_rgba(ca) == color_to_linear_rgba(cb)
        }
        _ => false,
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn tonemapping_to_tag(tm: Tonemapping) -> u8 {
    match tm {
        Tonemapping::None => TONEMAPPING_NONE,
        Tonemapping::Linear => TONEMAPPING_LINEAR,
        Tonemapping::Reinhard => TONEMAPPING_REINHARD,
        Tonemapping::ReinhardLuminance => TONEMAPPING_REINHARD_LUMINANCE,
        Tonemapping::AcesFitted => TONEMAPPING_ACES_FITTED,
        Tonemapping::AgX => TONEMAPPING_AGX,
        Tonemapping::SomewhatBoringDisplayTransform => {
            TONEMAPPING_SOMEWHAT_BORING_DISPLAY_TRANSFORM
        }
        Tonemapping::TonyMcMapface => TONEMAPPING_TONY_MC_MAPFACE,
        Tonemapping::BlenderFilmic => TONEMAPPING_BLENDER_FILMIC,
        Tonemapping::KhronosPbrNeutral => TONEMAPPING_KHRONOS_PBR_NEUTRAL,
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn tag_to_tonemapping(tag: u8) -> Tonemapping {
    match tag {
        TONEMAPPING_NONE => Tonemapping::None,
        TONEMAPPING_LINEAR => Tonemapping::Linear,
        TONEMAPPING_REINHARD => Tonemapping::Reinhard,
        TONEMAPPING_REINHARD_LUMINANCE => Tonemapping::ReinhardLuminance,
        TONEMAPPING_ACES_FITTED => Tonemapping::AcesFitted,
        TONEMAPPING_AGX => Tonemapping::AgX,
        TONEMAPPING_SOMEWHAT_BORING_DISPLAY_TRANSFORM => {
            Tonemapping::SomewhatBoringDisplayTransform
        }
        TONEMAPPING_BLENDER_FILMIC => Tonemapping::BlenderFilmic,
        TONEMAPPING_KHRONOS_PBR_NEUTRAL => Tonemapping::KhronosPbrNeutral,
        _ => Tonemapping::TonyMcMapface,
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn transmission_quality_to_tag(q: ScreenSpaceTransmissionQuality) -> u8 {
    match q {
        ScreenSpaceTransmissionQuality::Low => TRANSMISSION_QUALITY_LOW,
        ScreenSpaceTransmissionQuality::Medium => TRANSMISSION_QUALITY_MEDIUM,
        ScreenSpaceTransmissionQuality::High => TRANSMISSION_QUALITY_HIGH,
        ScreenSpaceTransmissionQuality::Ultra => TRANSMISSION_QUALITY_ULTRA,
    }
}

#[cfg(feature = "bridge-light")]
#[inline]
fn tag_to_transmission_quality(tag: u8) -> ScreenSpaceTransmissionQuality {
    match tag {
        TRANSMISSION_QUALITY_LOW => ScreenSpaceTransmissionQuality::Low,
        TRANSMISSION_QUALITY_HIGH => ScreenSpaceTransmissionQuality::High,
        TRANSMISSION_QUALITY_ULTRA => ScreenSpaceTransmissionQuality::Ultra,
        _ => ScreenSpaceTransmissionQuality::Medium,
    }
}

// ---------------------------------------------------------------------------
// FfiCamera2d (`bridge-light`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`Camera2d`] and its
/// paired [`Camera`] / [`Hdr`] configuration.
///
/// Layout: `24` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-light")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiCamera2d")]
pub struct FfiCamera2d {
    /// Camera render order (`Camera::order`; higher orders render later / on top).
    pub order: i32,
    /// Clear color linear RGBA `[r, g, b, a]` when `clear_color_mode == CLEAR_COLOR_CUSTOM`.
    pub clear_color: [f32; 4],
    /// Whether this camera is active (`Camera::is_active`).
    pub is_active: FfiBool,
    /// Clear color mode discriminant (`CLEAR_COLOR_DEFAULT`, `CLEAR_COLOR_CUSTOM`, `CLEAR_COLOR_NONE`).
    pub clear_color_mode: u8,
    /// Whether face culling is inverted (`Camera::invert_culling`).
    pub invert_culling: FfiBool,
    /// Whether Bevy's [`Hdr`] marker component is attached to this camera entity.
    pub hdr: FfiBool,
}

#[cfg(feature = "bridge-light")]
impl Default for FfiCamera2d {
    fn default() -> Self {
        Self::from_bevy(&Camera::default(), false)
    }
}

#[cfg(feature = "bridge-light")]
impl FfiCamera2d {
    /// Projects a Bevy [`Camera`] and [`Hdr`] presence into [`FfiCamera2d`].
    pub fn from_bevy(camera: &Camera, has_hdr: bool) -> Self {
        let (clear_color_mode, clear_color) = clear_color_to_ffi(camera.clear_color);
        Self {
            order: camera.order.clamp(i32::MIN as isize, i32::MAX as isize) as i32,
            clear_color,
            is_active: FfiBool::from_bool(camera.is_active),
            clear_color_mode,
            invert_culling: FfiBool::from_bool(camera.invert_culling),
            hdr: FfiBool::from_bool(has_hdr),
        }
    }

    /// Builds a fresh Bevy [`Camera`] from this mirror.
    pub fn to_camera(&self) -> Camera {
        let mut cam = Camera::default();
        self.apply_to_camera(&mut cam);
        cam
    }

    /// Returns `true` if `camera`'s mirrored fields already match `self`.
    pub fn matches_camera(&self, camera: &Camera) -> bool {
        let desired_clear = ffi_to_clear_color(self.clear_color_mode, self.clear_color);
        camera.order == self.order as isize
            && camera.is_active == self.is_active.get()
            && clear_color_eq(camera.clear_color, desired_clear)
            && camera.invert_culling == self.invert_culling.get()
    }

    /// Applies mirror fields to `camera`, returning `true` if any field changed.
    pub fn apply_to_camera(&self, camera: &mut Camera) -> bool {
        if self.matches_camera(camera) {
            return false;
        }
        camera.order = self.order as isize;
        camera.is_active = self.is_active.get();
        camera.clear_color = ffi_to_clear_color(self.clear_color_mode, self.clear_color);
        camera.invert_culling = self.invert_culling.get();
        true
    }
}

#[cfg(feature = "bridge-light")]
fn spawn_camera_2d_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiCamera2d), Without<Camera2d>>,
) {
    for (entity, ffi) in &query {
        let mut ec = commands.entity(entity);
        ec.insert((Camera2d, ffi.to_camera()));
        if ffi.hdr.get() {
            ec.insert(Hdr);
        }
    }
}

#[cfg(feature = "bridge-light")]
type Camera2dPushQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static FfiCamera2d, &'static mut Camera, Has<Hdr>),
    (Changed<FfiCamera2d>, With<Camera2d>),
>;

#[cfg(feature = "bridge-light")]
fn push_camera_2d_to_bevy(mut commands: Commands, mut query: Camera2dPushQuery<'_, '_>) {
    for (entity, ffi, mut camera, has_hdr) in &mut query {
        if !ffi.matches_camera(&camera) {
            ffi.apply_to_camera(&mut camera);
        }
        if ffi.hdr.get() && !has_hdr {
            commands.entity(entity).insert(Hdr);
        } else if !ffi.hdr.get() && has_hdr {
            commands.entity(entity).remove::<Hdr>();
        }
    }
}

#[cfg(feature = "bridge-light")]
fn pull_camera_2d_from_bevy(
    mut query: Query<(&mut FfiCamera2d, Ref<Camera>, Has<Hdr>), With<Camera2d>>,
) {
    for (mut ffi, camera, has_hdr) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        if !camera.is_changed() && has_hdr == ffi.hdr.get() {
            continue;
        }
        let projected = FfiCamera2d::from_bevy(&camera, has_hdr);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiCamera3d (`bridge-light`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`Camera3d`] paired with
/// [`Camera`], [`Tonemapping`], [`DebandDither`], [`Exposure`], [`Hdr`], and optional
/// [`ScreenSpaceTransmission`].
///
/// Layout: `48` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-light")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiCamera3d")]
pub struct FfiCamera3d {
    /// Camera render order (`Camera::order`).
    pub order: i32,
    /// Clear color linear RGBA `[r, g, b, a]` when `clear_color_mode == CLEAR_COLOR_CUSTOM`.
    pub clear_color: [f32; 4],
    /// Reverse-Z depth clear value when `depth_load_op == DEPTH_LOAD_OP_CLEAR` (default `0.0`).
    pub depth_clear_value: f32,
    /// `Camera3d::depth_texture_usages` bitflags (`wgpu_types::TextureUsages::bits()`).
    pub depth_texture_usages: u32,
    /// `Exposure::ev100` (default `Exposure::EV100_BLENDER` = `9.7`).
    pub exposure_ev100: f32,
    /// `ScreenSpaceTransmission::steps` when `transmission_enabled` is true (default `1`).
    pub transmission_steps: u32,
    /// Whether this camera is active (`Camera::is_active`).
    pub is_active: FfiBool,
    /// Clear color mode discriminant (`CLEAR_COLOR_DEFAULT`, `CLEAR_COLOR_CUSTOM`, `CLEAR_COLOR_NONE`).
    pub clear_color_mode: u8,
    /// Whether face culling is inverted (`Camera::invert_culling`).
    pub invert_culling: FfiBool,
    /// Whether Bevy's [`Hdr`] marker component is attached to this camera entity.
    pub hdr: FfiBool,
    /// Depth load op discriminant (`DEPTH_LOAD_OP_CLEAR` or `DEPTH_LOAD_OP_LOAD`).
    pub depth_load_op: u8,
    /// [`Tonemapping`] discriminant (`TONEMAPPING_NONE..=TONEMAPPING_KHRONOS_PBR_NEUTRAL`).
    pub tonemapping: u8,
    /// [`DebandDither`] flag (`FfiBool::FALSE` = `Disabled`, `FfiBool::TRUE` = `Enabled`).
    pub deband_dither: FfiBool,
    /// Whether [`ScreenSpaceTransmission`] is attached to this camera entity.
    pub transmission_enabled: FfiBool,
    /// [`ScreenSpaceTransmissionQuality`] discriminant (`TRANSMISSION_QUALITY_LOW..=ULTRA`).
    pub transmission_quality: u8,
    /// Explicit zero-reserved alignment padding bytes.
    pub reserved0: [u8; 3],
}

#[cfg(feature = "bridge-light")]
impl Default for FfiCamera3d {
    fn default() -> Self {
        Self::from_bevy_parts(
            &Camera3d::default(),
            &Camera::default(),
            Some(&Tonemapping::default()),
            Some(&DebandDither::default()),
            None,
            Some(&Exposure::default()),
            false,
        )
    }
}

#[cfg(feature = "bridge-light")]
impl FfiCamera3d {
    /// Projects Bevy 3D camera components into [`FfiCamera3d`].
    pub fn from_bevy_parts(
        camera_3d: &Camera3d,
        camera: &Camera,
        tonemapping: Option<&Tonemapping>,
        deband_dither: Option<&DebandDither>,
        transmission: Option<&ScreenSpaceTransmission>,
        exposure: Option<&Exposure>,
        has_hdr: bool,
    ) -> Self {
        let (clear_color_mode, clear_color) = clear_color_to_ffi(camera.clear_color);
        let (depth_load_op, depth_clear_value) = match camera_3d.depth_load_op {
            Camera3dDepthLoadOp::Clear(v) => (DEPTH_LOAD_OP_CLEAR, v),
            Camera3dDepthLoadOp::Load => (DEPTH_LOAD_OP_LOAD, 0.0),
        };
        let (transmission_enabled, transmission_steps, transmission_quality) = match transmission {
            Some(sst) => (
                FfiBool::TRUE,
                sst.steps.min(u32::MAX as usize) as u32,
                transmission_quality_to_tag(sst.quality),
            ),
            None => (
                FfiBool::FALSE,
                ScreenSpaceTransmission::default().steps as u32,
                transmission_quality_to_tag(ScreenSpaceTransmissionQuality::default()),
            ),
        };
        Self {
            order: camera.order.clamp(i32::MIN as isize, i32::MAX as isize) as i32,
            clear_color,
            depth_clear_value,
            depth_texture_usages: camera_3d.depth_texture_usages.0,
            exposure_ev100: exposure.map_or(Exposure::EV100_BLENDER, |e| e.ev100),
            transmission_steps,
            is_active: FfiBool::from_bool(camera.is_active),
            clear_color_mode,
            invert_culling: FfiBool::from_bool(camera.invert_culling),
            hdr: FfiBool::from_bool(has_hdr),
            depth_load_op,
            tonemapping: tonemapping_to_tag(tonemapping.copied().unwrap_or_default()),
            deband_dither: FfiBool::from_bool(matches!(
                deband_dither.copied().unwrap_or_default(),
                DebandDither::Enabled
            )),
            transmission_enabled,
            transmission_quality,
            reserved0: [0; 3],
        }
    }

    /// Builds a Bevy [`Camera3d`] component from this mirror.
    pub fn to_camera_3d(&self) -> Camera3d {
        let depth_load_op = if self.depth_load_op == DEPTH_LOAD_OP_LOAD {
            Camera3dDepthLoadOp::Load
        } else {
            let val = if self.depth_clear_value.is_finite() {
                self.depth_clear_value
            } else {
                0.0
            };
            Camera3dDepthLoadOp::Clear(val)
        };
        let usages = if self.depth_texture_usages == 0 {
            Camera3d::default().depth_texture_usages
        } else {
            Camera3dDepthTextureUsage(self.depth_texture_usages)
        };
        Camera3d {
            depth_load_op,
            depth_texture_usages: usages,
        }
    }

    /// Builds a Bevy [`Camera`] component from this mirror.
    pub fn to_camera(&self) -> Camera {
        let mut cam = Camera::default();
        self.apply_to_camera(&mut cam);
        cam
    }

    /// Returns `true` if `camera`'s mirrored fields already match `self`.
    pub fn matches_camera(&self, camera: &Camera) -> bool {
        let desired_clear = ffi_to_clear_color(self.clear_color_mode, self.clear_color);
        camera.order == self.order as isize
            && camera.is_active == self.is_active.get()
            && clear_color_eq(camera.clear_color, desired_clear)
            && camera.invert_culling == self.invert_culling.get()
    }

    /// Applies camera fields to `camera`, returning `true` if any field changed.
    pub fn apply_to_camera(&self, camera: &mut Camera) -> bool {
        if self.matches_camera(camera) {
            return false;
        }
        camera.order = self.order as isize;
        camera.is_active = self.is_active.get();
        camera.clear_color = ffi_to_clear_color(self.clear_color_mode, self.clear_color);
        camera.invert_culling = self.invert_culling.get();
        true
    }

    /// Decodes the mirrored [`Tonemapping`] enum.
    #[inline]
    pub fn to_tonemapping(&self) -> Tonemapping {
        tag_to_tonemapping(self.tonemapping)
    }

    /// Decodes the mirrored [`DebandDither`] enum.
    #[inline]
    pub fn to_deband_dither(&self) -> DebandDither {
        if self.deband_dither.get() {
            DebandDither::Enabled
        } else {
            DebandDither::Disabled
        }
    }

    /// Decodes the optional [`ScreenSpaceTransmission`] component.
    #[inline]
    pub fn to_screen_space_transmission(&self) -> Option<ScreenSpaceTransmission> {
        self.transmission_enabled
            .get()
            .then(|| ScreenSpaceTransmission {
                steps: self.transmission_steps as usize,
                quality: tag_to_transmission_quality(self.transmission_quality),
            })
    }

    /// Decodes the mirrored [`Exposure`] component.
    #[inline]
    pub fn to_exposure(&self) -> Exposure {
        Exposure {
            ev100: if self.exposure_ev100.is_finite() {
                self.exposure_ev100
            } else {
                Exposure::EV100_BLENDER
            },
        }
    }
}

#[cfg(feature = "bridge-light")]
fn camera_3d_eq(a: &Camera3d, b: &Camera3d) -> bool {
    let op_eq = match (&a.depth_load_op, &b.depth_load_op) {
        (Camera3dDepthLoadOp::Clear(va), Camera3dDepthLoadOp::Clear(vb)) => va == vb,
        (Camera3dDepthLoadOp::Load, Camera3dDepthLoadOp::Load) => true,
        _ => false,
    };
    op_eq && a.depth_texture_usages.0 == b.depth_texture_usages.0
}

#[cfg(feature = "bridge-light")]
fn spawn_camera_3d_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiCamera3d), Without<Camera3d>>,
) {
    for (entity, ffi) in &query {
        let mut ec = commands.entity(entity);
        ec.insert((
            ffi.to_camera_3d(),
            ffi.to_camera(),
            ffi.to_tonemapping(),
            ffi.to_deband_dither(),
            ffi.to_exposure(),
        ));
        if ffi.hdr.get() {
            ec.insert(Hdr);
        }
        if let Some(sst) = ffi.to_screen_space_transmission() {
            ec.insert(sst);
        }
    }
}

#[cfg(feature = "bridge-light")]
type Camera3dPushQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static FfiCamera3d,
        &'static mut Camera3d,
        &'static mut Camera,
        Option<&'static mut Tonemapping>,
        Option<&'static mut DebandDither>,
        Option<&'static mut ScreenSpaceTransmission>,
        Option<&'static mut Exposure>,
        Has<Hdr>,
    ),
    Changed<FfiCamera3d>,
>;

#[cfg(feature = "bridge-light")]
fn push_camera_3d_to_bevy(mut commands: Commands, mut query: Camera3dPushQuery<'_, '_>) {
    for (entity, ffi, mut cam3d, mut cam, tonemapping, deband, transmission, exposure, has_hdr) in
        &mut query
    {
        let desired_cam3d = ffi.to_camera_3d();
        if !camera_3d_eq(&cam3d, &desired_cam3d) {
            *cam3d = desired_cam3d;
        }
        if !ffi.matches_camera(&cam) {
            ffi.apply_to_camera(&mut cam);
        }

        let desired_tm = ffi.to_tonemapping();
        match tonemapping {
            Some(mut existing) => {
                if *existing != desired_tm {
                    *existing = desired_tm;
                }
            }
            None => {
                commands.entity(entity).insert(desired_tm);
            }
        }

        let desired_deband = ffi.to_deband_dither();
        match deband {
            Some(mut existing) => {
                if *existing != desired_deband {
                    *existing = desired_deband;
                }
            }
            None => {
                commands.entity(entity).insert(desired_deband);
            }
        }

        let desired_exp = ffi.to_exposure();
        match exposure {
            Some(mut existing) => {
                if existing.ev100 != desired_exp.ev100 {
                    *existing = desired_exp;
                }
            }
            None => {
                commands.entity(entity).insert(desired_exp);
            }
        }

        if ffi.hdr.get() && !has_hdr {
            commands.entity(entity).insert(Hdr);
        } else if !ffi.hdr.get() && has_hdr {
            commands.entity(entity).remove::<Hdr>();
        }

        match (ffi.to_screen_space_transmission(), transmission) {
            (Some(desired_sst), Some(mut existing)) => {
                if existing.steps != desired_sst.steps || existing.quality != desired_sst.quality {
                    *existing = desired_sst;
                }
            }
            (Some(desired_sst), None) => {
                commands.entity(entity).insert(desired_sst);
            }
            (None, Some(_)) => {
                commands.entity(entity).remove::<ScreenSpaceTransmission>();
            }
            (None, None) => {}
        }
    }
}

#[cfg(feature = "bridge-light")]
type Camera3dPullQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut FfiCamera3d,
        Ref<'static, Camera3d>,
        Ref<'static, Camera>,
        Option<Ref<'static, Tonemapping>>,
        Option<Ref<'static, DebandDither>>,
        Option<Ref<'static, ScreenSpaceTransmission>>,
        Option<Ref<'static, Exposure>>,
        Has<Hdr>,
    ),
>;

#[cfg(feature = "bridge-light")]
fn pull_camera_3d_from_bevy(mut query: Camera3dPullQuery<'_, '_>) {
    for (mut ffi, cam3d, cam, tonemapping, deband, transmission, exposure, has_hdr) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let any_bevy_changed = cam3d.is_changed()
            || cam.is_changed()
            || tonemapping.as_ref().is_some_and(DetectChanges::is_changed)
            || deband.as_ref().is_some_and(DetectChanges::is_changed)
            || transmission.as_ref().is_some_and(DetectChanges::is_changed)
            || exposure.as_ref().is_some_and(DetectChanges::is_changed)
            || has_hdr != ffi.hdr.get()
            || transmission.is_some() != ffi.transmission_enabled.get();
        if !any_bevy_changed {
            continue;
        }
        let projected = FfiCamera3d::from_bevy_parts(
            &cam3d,
            &cam,
            tonemapping.as_deref(),
            deband.as_deref(),
            transmission.as_deref(),
            exposure.as_deref(),
            has_hdr,
        );
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiPointLight (`bridge-light`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`PointLight`].
///
/// Layout: `44` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-light")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiPointLight")]
pub struct FfiPointLight {
    /// Linear RGBA color `[r, g, b, a]`.
    pub color: [f32; 4],
    /// Luminous power in lumens.
    pub intensity: f32,
    /// Cut-off range in meters.
    pub range: f32,
    /// Spherical emitter radius in meters.
    pub radius: f32,
    /// Shadow map depth bias.
    pub shadow_depth_bias: f32,
    /// Shadow map normal bias.
    pub shadow_normal_bias: f32,
    /// Shadow map near-Z plane distance.
    pub shadow_map_near_z: f32,
    /// Whether shadow maps are enabled (`PointLight::shadow_maps_enabled`).
    pub shadow_maps_enabled: FfiBool,
    /// Whether contact shadows are enabled (`PointLight::contact_shadows_enabled`).
    pub contact_shadows_enabled: FfiBool,
    /// Whether this light contributes diffuse lighting to lightmapped meshes.
    pub affects_lightmapped_mesh_diffuse: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

#[cfg(feature = "bridge-light")]
impl Default for FfiPointLight {
    fn default() -> Self {
        Self::from_bevy(&PointLight::default())
    }
}

#[cfg(feature = "bridge-light")]
impl FfiPointLight {
    /// Projects a Bevy [`PointLight`] into [`FfiPointLight`].
    pub fn from_bevy(light: &PointLight) -> Self {
        Self {
            color: color_to_linear_rgba(light.color),
            intensity: light.intensity,
            range: light.range,
            radius: light.radius,
            shadow_depth_bias: light.shadow_depth_bias,
            shadow_normal_bias: light.shadow_normal_bias,
            shadow_map_near_z: light.shadow_map_near_z,
            shadow_maps_enabled: FfiBool::from_bool(light.shadow_maps_enabled),
            contact_shadows_enabled: FfiBool::from_bool(light.contact_shadows_enabled),
            affects_lightmapped_mesh_diffuse: FfiBool::from_bool(
                light.affects_lightmapped_mesh_diffuse,
            ),
            reserved0: 0,
        }
    }

    /// Builds a Bevy [`PointLight`] from this mirror.
    pub fn to_bevy(&self) -> PointLight {
        let mut light = PointLight::default();
        self.apply_to_bevy(&mut light);
        light
    }

    /// Applies mirror fields onto `dst` (ignoring non-finite floats).
    pub fn apply_to_bevy(&self, dst: &mut PointLight) {
        if let Some(c) = linear_rgba_to_color(self.color) {
            dst.color = c;
        }
        if self.intensity.is_finite() {
            dst.intensity = self.intensity;
        }
        if self.range.is_finite() {
            dst.range = self.range;
        }
        if self.radius.is_finite() {
            dst.radius = self.radius;
        }
        if self.shadow_depth_bias.is_finite() {
            dst.shadow_depth_bias = self.shadow_depth_bias;
        }
        if self.shadow_normal_bias.is_finite() {
            dst.shadow_normal_bias = self.shadow_normal_bias;
        }
        if self.shadow_map_near_z.is_finite() {
            dst.shadow_map_near_z = self.shadow_map_near_z;
        }
        dst.shadow_maps_enabled = self.shadow_maps_enabled.get();
        dst.contact_shadows_enabled = self.contact_shadows_enabled.get();
        dst.affects_lightmapped_mesh_diffuse = self.affects_lightmapped_mesh_diffuse.get();
    }
}

#[cfg(feature = "bridge-light")]
fn spawn_point_light_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiPointLight), Without<PointLight>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

#[cfg(feature = "bridge-light")]
fn push_point_light_to_bevy(
    mut query: Query<(&FfiPointLight, &mut PointLight), Changed<FfiPointLight>>,
) {
    for (ffi, mut light) in &mut query {
        if FfiPointLight::from_bevy(&light) != *ffi {
            ffi.apply_to_bevy(&mut light);
        }
    }
}

#[cfg(feature = "bridge-light")]
fn pull_point_light_from_bevy(
    mut query: Query<(&mut FfiPointLight, Ref<PointLight>), Changed<PointLight>>,
) {
    for (mut ffi, light) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiPointLight::from_bevy(&light);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiSpotLight (`bridge-light`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`SpotLight`].
///
/// Layout: `52` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-light")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiSpotLight")]
pub struct FfiSpotLight {
    /// Linear RGBA color `[r, g, b, a]`.
    pub color: [f32; 4],
    /// Luminous power in lumens.
    pub intensity: f32,
    /// Illumination range in meters.
    pub range: f32,
    /// Spherical emitter radius in meters.
    pub radius: f32,
    /// Shadow map depth bias.
    pub shadow_depth_bias: f32,
    /// Shadow map normal bias.
    pub shadow_normal_bias: f32,
    /// Shadow map near-Z plane distance.
    pub shadow_map_near_z: f32,
    /// Outer cone half-angle in radians (`< PI / 2`).
    pub outer_angle: f32,
    /// Inner cone half-angle in radians (`<= outer_angle`).
    pub inner_angle: f32,
    /// Whether shadow maps are enabled (`SpotLight::shadow_maps_enabled`).
    pub shadow_maps_enabled: FfiBool,
    /// Whether contact shadows are enabled (`SpotLight::contact_shadows_enabled`).
    pub contact_shadows_enabled: FfiBool,
    /// Whether this light contributes diffuse lighting to lightmapped meshes.
    pub affects_lightmapped_mesh_diffuse: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

#[cfg(feature = "bridge-light")]
impl Default for FfiSpotLight {
    fn default() -> Self {
        Self::from_bevy(&SpotLight::default())
    }
}

#[cfg(feature = "bridge-light")]
impl FfiSpotLight {
    /// Projects a Bevy [`SpotLight`] into [`FfiSpotLight`].
    pub fn from_bevy(light: &SpotLight) -> Self {
        Self {
            color: color_to_linear_rgba(light.color),
            intensity: light.intensity,
            range: light.range,
            radius: light.radius,
            shadow_depth_bias: light.shadow_depth_bias,
            shadow_normal_bias: light.shadow_normal_bias,
            shadow_map_near_z: light.shadow_map_near_z,
            outer_angle: light.outer_angle,
            inner_angle: light.inner_angle,
            shadow_maps_enabled: FfiBool::from_bool(light.shadow_maps_enabled),
            contact_shadows_enabled: FfiBool::from_bool(light.contact_shadows_enabled),
            affects_lightmapped_mesh_diffuse: FfiBool::from_bool(
                light.affects_lightmapped_mesh_diffuse,
            ),
            reserved0: 0,
        }
    }

    /// Builds a Bevy [`SpotLight`] from this mirror.
    pub fn to_bevy(&self) -> SpotLight {
        let mut light = SpotLight::default();
        self.apply_to_bevy(&mut light);
        light
    }

    /// Applies mirror fields onto `dst` (ignoring non-finite floats).
    pub fn apply_to_bevy(&self, dst: &mut SpotLight) {
        if let Some(c) = linear_rgba_to_color(self.color) {
            dst.color = c;
        }
        if self.intensity.is_finite() {
            dst.intensity = self.intensity;
        }
        if self.range.is_finite() {
            dst.range = self.range;
        }
        if self.radius.is_finite() {
            dst.radius = self.radius;
        }
        if self.shadow_depth_bias.is_finite() {
            dst.shadow_depth_bias = self.shadow_depth_bias;
        }
        if self.shadow_normal_bias.is_finite() {
            dst.shadow_normal_bias = self.shadow_normal_bias;
        }
        if self.shadow_map_near_z.is_finite() {
            dst.shadow_map_near_z = self.shadow_map_near_z;
        }
        if self.outer_angle.is_finite() {
            dst.outer_angle = self.outer_angle;
        }
        if self.inner_angle.is_finite() {
            dst.inner_angle = self.inner_angle;
        }
        dst.shadow_maps_enabled = self.shadow_maps_enabled.get();
        dst.contact_shadows_enabled = self.contact_shadows_enabled.get();
        dst.affects_lightmapped_mesh_diffuse = self.affects_lightmapped_mesh_diffuse.get();
    }
}

#[cfg(feature = "bridge-light")]
fn spawn_spot_light_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiSpotLight), Without<SpotLight>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

#[cfg(feature = "bridge-light")]
fn push_spot_light_to_bevy(
    mut query: Query<(&FfiSpotLight, &mut SpotLight), Changed<FfiSpotLight>>,
) {
    for (ffi, mut light) in &mut query {
        if FfiSpotLight::from_bevy(&light) != *ffi {
            ffi.apply_to_bevy(&mut light);
        }
    }
}

#[cfg(feature = "bridge-light")]
fn pull_spot_light_from_bevy(
    mut query: Query<(&mut FfiSpotLight, Ref<SpotLight>), Changed<SpotLight>>,
) {
    for (mut ffi, light) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiSpotLight::from_bevy(&light);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiDirectionalLight (`bridge-light`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`DirectionalLight`].
///
/// Layout: `32` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-light")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiDirectionalLight")]
pub struct FfiDirectionalLight {
    /// Linear RGBA color `[r, g, b, a]`.
    pub color: [f32; 4],
    /// Illuminance in lux.
    pub illuminance: f32,
    /// Cascaded shadow map depth bias.
    pub shadow_depth_bias: f32,
    /// Cascaded shadow map normal bias.
    pub shadow_normal_bias: f32,
    /// Whether shadow maps are enabled (`DirectionalLight::shadow_maps_enabled`).
    pub shadow_maps_enabled: FfiBool,
    /// Whether contact shadows are enabled (`DirectionalLight::contact_shadows_enabled`).
    pub contact_shadows_enabled: FfiBool,
    /// Whether this light contributes diffuse lighting to lightmapped meshes.
    pub affects_lightmapped_mesh_diffuse: FfiBool,
    /// Explicit zero-reserved alignment byte.
    pub reserved0: u8,
}

#[cfg(feature = "bridge-light")]
impl Default for FfiDirectionalLight {
    fn default() -> Self {
        Self::from_bevy(&DirectionalLight::default())
    }
}

#[cfg(feature = "bridge-light")]
impl FfiDirectionalLight {
    /// Projects a Bevy [`DirectionalLight`] into [`FfiDirectionalLight`].
    pub fn from_bevy(light: &DirectionalLight) -> Self {
        Self {
            color: color_to_linear_rgba(light.color),
            illuminance: light.illuminance,
            shadow_depth_bias: light.shadow_depth_bias,
            shadow_normal_bias: light.shadow_normal_bias,
            shadow_maps_enabled: FfiBool::from_bool(light.shadow_maps_enabled),
            contact_shadows_enabled: FfiBool::from_bool(light.contact_shadows_enabled),
            affects_lightmapped_mesh_diffuse: FfiBool::from_bool(
                light.affects_lightmapped_mesh_diffuse,
            ),
            reserved0: 0,
        }
    }

    /// Builds a Bevy [`DirectionalLight`] from this mirror.
    pub fn to_bevy(&self) -> DirectionalLight {
        let mut light = DirectionalLight::default();
        self.apply_to_bevy(&mut light);
        light
    }

    /// Applies mirror fields onto `dst` (ignoring non-finite floats).
    pub fn apply_to_bevy(&self, dst: &mut DirectionalLight) {
        if let Some(c) = linear_rgba_to_color(self.color) {
            dst.color = c;
        }
        if self.illuminance.is_finite() {
            dst.illuminance = self.illuminance;
        }
        if self.shadow_depth_bias.is_finite() {
            dst.shadow_depth_bias = self.shadow_depth_bias;
        }
        if self.shadow_normal_bias.is_finite() {
            dst.shadow_normal_bias = self.shadow_normal_bias;
        }
        dst.shadow_maps_enabled = self.shadow_maps_enabled.get();
        dst.contact_shadows_enabled = self.contact_shadows_enabled.get();
        dst.affects_lightmapped_mesh_diffuse = self.affects_lightmapped_mesh_diffuse.get();
    }
}

#[cfg(feature = "bridge-light")]
fn spawn_directional_light_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiDirectionalLight), Without<DirectionalLight>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

#[cfg(feature = "bridge-light")]
fn push_directional_light_to_bevy(
    mut query: Query<(&FfiDirectionalLight, &mut DirectionalLight), Changed<FfiDirectionalLight>>,
) {
    for (ffi, mut light) in &mut query {
        if FfiDirectionalLight::from_bevy(&light) != *ffi {
            ffi.apply_to_bevy(&mut light);
        }
    }
}

#[cfg(feature = "bridge-light")]
fn pull_directional_light_from_bevy(
    mut query: Query<(&mut FfiDirectionalLight, Ref<DirectionalLight>), Changed<DirectionalLight>>,
) {
    for (mut ffi, light) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiDirectionalLight::from_bevy(&light);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiPanOrbitCamera (`bridge-camera-controller`)
// ---------------------------------------------------------------------------

#[cfg(feature = "bridge-camera-controller")]
#[inline]
fn secs_to_duration(secs: f32) -> Duration {
    if secs.is_finite() && secs >= 0.0 {
        Duration::from_secs_f32(secs)
    } else {
        Duration::ZERO
    }
}

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy `0.20`'s upstreamed
/// [`PanOrbitCamera`] controller configuration.
///
/// Preserves runtime `current_motion` on the host [`PanOrbitCamera`] while bidirectionally
/// syncing motion enable flags, orbit constraints, zoom limits, smoothing windows, sensitivities,
/// momentum parameters, debounce, projection clip settings, and `last_anchor_depth`.
///
/// Layout: `136` bytes, `8`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-camera-controller")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiPanOrbitCamera")]
pub struct FfiPanOrbitCamera {
    /// Fallback anchor depth along camera view space Z (`PanOrbitCamera::last_anchor_depth`).
    pub last_anchor_depth: f64,
    /// Minimum world units per pixel when zooming in (`ZoomLimits::min_size_per_pixel`).
    pub zoom_min_size_per_pixel: f64,
    /// Maximum world units per pixel when zooming out (`ZoomLimits::max_size_per_pixel`).
    pub zoom_max_size_per_pixel: f64,
    /// Fixed up-axis unit vector `[x, y, z]` when `orbit_constraint_mode == ORBIT_CONSTRAINT_FIXED`.
    pub orbit_up: [f64; 3],
    /// Constant pan momentum friction (`Momentum::pan_friction`).
    pub momentum_pan_friction: f64,
    /// Constant orbit momentum friction (`Momentum::orbit_friction`).
    pub momentum_orbit_friction: f64,
    /// Pan input smoothing window in seconds (`Smoothing::pan`).
    pub smoothing_pan_secs: f32,
    /// Orbit input smoothing window in seconds (`Smoothing::orbit`).
    pub smoothing_orbit_secs: f32,
    /// Zoom input smoothing window in seconds (`Smoothing::zoom`).
    pub smoothing_zoom_secs: f32,
    /// Orbit input sensitivity `[x, y]` (`Sensitivity::orbit`).
    pub sensitivity_orbit: [f32; 2],
    /// Zoom input sensitivity (`Sensitivity::zoom`).
    pub sensitivity_zoom: f32,
    /// Pan momentum initial sampling window in seconds (`Momentum::init_pan`).
    pub momentum_init_pan_secs: f32,
    /// Orbit momentum initial sampling window in seconds (`Momentum::init_orbit`).
    pub momentum_init_orbit_secs: f32,
    /// Debounce window in seconds before starting a new motion (`PanOrbitCamera::input_debounce`).
    pub input_debounce_secs: f32,
    /// Lower bound of perspective near-clip range (`PerspectiveSettings::near_clip_limits.start`).
    pub perspective_near_clip_min: f32,
    /// Upper bound of perspective near-clip range (`PerspectiveSettings::near_clip_limits.end`).
    pub perspective_near_clip_max: f32,
    /// Perspective near-clip multiplier (`PerspectiveSettings::near_clip_multiplier`).
    pub perspective_near_clip_multiplier: f32,
    /// Orthographic scale-to-near-clip factor (`OrthographicSettings::scale_to_near_clip`).
    pub ortho_scale_to_near_clip: f32,
    /// Lower bound of orthographic near-clip range (`OrthographicSettings::near_clip_limits.start`).
    pub ortho_near_clip_min: f32,
    /// Upper bound of orthographic near-clip range (`OrthographicSettings::near_clip_limits.end`).
    pub ortho_near_clip_max: f32,
    /// Orthographic far-clip multiplier (`OrthographicSettings::far_clip_multiplier`).
    pub ortho_far_clip_multiplier: f32,
    /// Whether panning is enabled (`EnabledMotion::pan`).
    pub enabled_pan: FfiBool,
    /// Whether orbiting is enabled (`EnabledMotion::orbit`).
    pub enabled_orbit: FfiBool,
    /// Whether zooming is enabled (`EnabledMotion::zoom`).
    pub enabled_zoom: FfiBool,
    /// Orbit constraint discriminant (`ORBIT_CONSTRAINT_FIXED` or `ORBIT_CONSTRAINT_FREE`).
    pub orbit_constraint_mode: u8,
    /// Whether fixed-up orbit may pass over top dead center (`OrbitConstraint::Fixed::can_pass_tdc`).
    pub orbit_can_pass_tdc: FfiBool,
    /// Whether zooming in may pass through objects (`ZoomLimits::zoom_through_objects`).
    pub zoom_through_objects: FfiBool,
    /// Velocity-proportional pan momentum damping (`Momentum::pan_damping`).
    pub momentum_pan_damping: u8,
    /// Velocity-proportional orbit momentum damping (`Momentum::orbit_damping`).
    pub momentum_orbit_damping: u8,
}

#[cfg(feature = "bridge-camera-controller")]
impl Default for FfiPanOrbitCamera {
    fn default() -> Self {
        Self::from_bevy(&PanOrbitCamera::default())
    }
}

#[cfg(feature = "bridge-camera-controller")]
impl FfiPanOrbitCamera {
    /// Projects a Bevy [`PanOrbitCamera`] into [`FfiPanOrbitCamera`].
    pub fn from_bevy(cam: &PanOrbitCamera) -> Self {
        let (orbit_constraint_mode, orbit_up, orbit_can_pass_tdc) = match cam.orbit_constraint {
            OrbitConstraint::Fixed { up, can_pass_tdc } => (
                ORBIT_CONSTRAINT_FIXED,
                up.to_array(),
                FfiBool::from_bool(can_pass_tdc),
            ),
            OrbitConstraint::Free => (ORBIT_CONSTRAINT_FREE, DVec3::Y.to_array(), FfiBool::FALSE),
        };
        Self {
            last_anchor_depth: cam.last_anchor_depth,
            zoom_min_size_per_pixel: cam.zoom_limits.min_size_per_pixel,
            zoom_max_size_per_pixel: cam.zoom_limits.max_size_per_pixel,
            orbit_up,
            momentum_pan_friction: cam.momentum.pan_friction,
            momentum_orbit_friction: cam.momentum.orbit_friction,
            smoothing_pan_secs: cam.smoothing.pan.as_secs_f32(),
            smoothing_orbit_secs: cam.smoothing.orbit.as_secs_f32(),
            smoothing_zoom_secs: cam.smoothing.zoom.as_secs_f32(),
            sensitivity_orbit: cam.sensitivity.orbit.to_array(),
            sensitivity_zoom: cam.sensitivity.zoom,
            momentum_init_pan_secs: cam.momentum.init_pan.as_secs_f32(),
            momentum_init_orbit_secs: cam.momentum.init_orbit.as_secs_f32(),
            input_debounce_secs: cam.input_debounce.as_secs_f32(),
            perspective_near_clip_min: cam.perspective.near_clip_limits.start,
            perspective_near_clip_max: cam.perspective.near_clip_limits.end,
            perspective_near_clip_multiplier: cam.perspective.near_clip_multiplier,
            ortho_scale_to_near_clip: cam.orthographic.scale_to_near_clip,
            ortho_near_clip_min: cam.orthographic.near_clip_limits.start,
            ortho_near_clip_max: cam.orthographic.near_clip_limits.end,
            ortho_far_clip_multiplier: cam.orthographic.far_clip_multiplier,
            enabled_pan: FfiBool::from_bool(cam.enabled_motion.pan),
            enabled_orbit: FfiBool::from_bool(cam.enabled_motion.orbit),
            enabled_zoom: FfiBool::from_bool(cam.enabled_motion.zoom),
            orbit_constraint_mode,
            orbit_can_pass_tdc,
            zoom_through_objects: FfiBool::from_bool(cam.zoom_limits.zoom_through_objects),
            momentum_pan_damping: cam.momentum.pan_damping,
            momentum_orbit_damping: cam.momentum.orbit_damping,
        }
    }

    /// Constructs a new Bevy [`PanOrbitCamera`] from this mirror.
    pub fn to_bevy(&self) -> PanOrbitCamera {
        let mut cam = PanOrbitCamera::default();
        self.apply_to_bevy(&mut cam);
        cam
    }

    /// Applies mirror configuration fields onto `dst` while preserving `dst.current_motion`.
    pub fn apply_to_bevy(&self, dst: &mut PanOrbitCamera) {
        dst.enabled_motion = EnabledMotion {
            pan: self.enabled_pan.get(),
            orbit: self.enabled_orbit.get(),
            zoom: self.enabled_zoom.get(),
        };
        dst.orbit_constraint = if self.orbit_constraint_mode == ORBIT_CONSTRAINT_FREE {
            OrbitConstraint::Free
        } else {
            let raw_up = DVec3::from_array(self.orbit_up);
            let up = if raw_up.is_finite() {
                raw_up.try_normalize().unwrap_or(DVec3::Y)
            } else {
                DVec3::Y
            };
            OrbitConstraint::Fixed {
                up,
                can_pass_tdc: self.orbit_can_pass_tdc.get(),
            }
        };
        dst.zoom_limits = ZoomLimits {
            min_size_per_pixel: if self.zoom_min_size_per_pixel.is_finite() {
                self.zoom_min_size_per_pixel
            } else {
                dst.zoom_limits.min_size_per_pixel
            },
            max_size_per_pixel: if self.zoom_max_size_per_pixel.is_finite() {
                self.zoom_max_size_per_pixel
            } else {
                dst.zoom_limits.max_size_per_pixel
            },
            zoom_through_objects: self.zoom_through_objects.get(),
        };
        dst.smoothing = Smoothing {
            pan: secs_to_duration(self.smoothing_pan_secs),
            orbit: secs_to_duration(self.smoothing_orbit_secs),
            zoom: secs_to_duration(self.smoothing_zoom_secs),
        };
        dst.sensitivity = Sensitivity {
            orbit: if self.sensitivity_orbit.iter().all(|v| v.is_finite()) {
                Vec2::from_array(self.sensitivity_orbit)
            } else {
                dst.sensitivity.orbit
            },
            zoom: if self.sensitivity_zoom.is_finite() {
                self.sensitivity_zoom
            } else {
                dst.sensitivity.zoom
            },
        };
        dst.momentum = Momentum {
            pan_damping: self.momentum_pan_damping,
            pan_friction: if self.momentum_pan_friction.is_finite() {
                self.momentum_pan_friction
            } else {
                dst.momentum.pan_friction
            },
            init_pan: secs_to_duration(self.momentum_init_pan_secs),
            orbit_damping: self.momentum_orbit_damping,
            orbit_friction: if self.momentum_orbit_friction.is_finite() {
                self.momentum_orbit_friction
            } else {
                dst.momentum.orbit_friction
            },
            init_orbit: secs_to_duration(self.momentum_init_orbit_secs),
        };
        dst.input_debounce = secs_to_duration(self.input_debounce_secs);
        dst.perspective = PerspectiveSettings {
            near_clip_limits: {
                let start = if !self.perspective_near_clip_min.is_nan() {
                    self.perspective_near_clip_min
                } else {
                    dst.perspective.near_clip_limits.start
                };
                let end = if !self.perspective_near_clip_max.is_nan() {
                    self.perspective_near_clip_max
                } else {
                    dst.perspective.near_clip_limits.end
                };
                start..end
            },
            near_clip_multiplier: if self.perspective_near_clip_multiplier.is_finite() {
                self.perspective_near_clip_multiplier
            } else {
                dst.perspective.near_clip_multiplier
            },
        };
        dst.orthographic = OrthographicSettings {
            scale_to_near_clip: if self.ortho_scale_to_near_clip.is_finite() {
                self.ortho_scale_to_near_clip
            } else {
                dst.orthographic.scale_to_near_clip
            },
            near_clip_limits: {
                let start = if !self.ortho_near_clip_min.is_nan() {
                    self.ortho_near_clip_min
                } else {
                    dst.orthographic.near_clip_limits.start
                };
                let end = if !self.ortho_near_clip_max.is_nan() {
                    self.ortho_near_clip_max
                } else {
                    dst.orthographic.near_clip_limits.end
                };
                start..end
            },
            far_clip_multiplier: if self.ortho_far_clip_multiplier.is_finite() {
                self.ortho_far_clip_multiplier
            } else {
                dst.orthographic.far_clip_multiplier
            },
        };
        if self.last_anchor_depth.is_finite() {
            dst.last_anchor_depth = self.last_anchor_depth;
        }
    }
}

#[cfg(feature = "bridge-camera-controller")]
fn spawn_pan_orbit_camera_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiPanOrbitCamera), Without<PanOrbitCamera>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

#[cfg(feature = "bridge-camera-controller")]
fn push_pan_orbit_camera_to_bevy(
    mut query: Query<(&FfiPanOrbitCamera, &mut PanOrbitCamera), Changed<FfiPanOrbitCamera>>,
) {
    for (ffi, mut cam) in &mut query {
        if FfiPanOrbitCamera::from_bevy(&cam) != *ffi {
            ffi.apply_to_bevy(&mut cam);
        }
    }
}

#[cfg(feature = "bridge-camera-controller")]
fn pull_pan_orbit_camera_from_bevy(
    mut query: Query<(&mut FfiPanOrbitCamera, Ref<PanOrbitCamera>), Changed<PanOrbitCamera>>,
) {
    for (mut ffi, cam) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiPanOrbitCamera::from_bevy(&cam);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiSolariLighting (`bridge-solari`)
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy `0.20`'s
/// [`SolariLighting`] raytraced global illumination component.
///
/// Covers all 12 runtime fields of `bevy_solari::realtime::SolariLighting` (`restir`,
/// `confidence_weight_cap`, `primary_di_samples`, `secondary_di_samples`, `max_bounces`, world
/// cache sampling/LOD controls, and `reset`).
///
/// Layout: `44` bytes, `4`-byte alignment, zero implicit compiler padding.
#[cfg(feature = "bridge-solari")]
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::camera_light::FfiSolariLighting")]
pub struct FfiSolariLighting {
    /// Maximum ReSTIR confidence weight (`SolariLighting::confidence_weight_cap`).
    pub confidence_weight_cap: f32,
    /// Primary hit direct light sample count (`SolariLighting::primary_di_samples`).
    pub primary_di_samples: u32,
    /// Indirect bounce direct light sample count (`SolariLighting::secondary_di_samples`).
    pub secondary_di_samples: u32,
    /// Maximum path bounce count (`SolariLighting::max_bounces`).
    pub max_bounces: u32,
    /// World cache temporal responsiveness (`SolariLighting::world_cache_max_temporal_samples`).
    pub world_cache_max_temporal_samples: f32,
    /// Direct light samples per world cache cell (`SolariLighting::world_cache_direct_light_sample_count`).
    pub world_cache_direct_light_sample_count: u32,
    /// Maximum GI ray distance between world cache cells (`SolariLighting::world_cache_max_gi_ray_distance`).
    pub world_cache_max_gi_ray_distance: f32,
    /// Stochastic soft target for world cache cell updates per frame (`SolariLighting::world_cache_cell_updates_soft_target`).
    pub world_cache_cell_updates_soft_target: u32,
    /// Base world cache cell size in meters (`SolariLighting::world_cache_position_base_cell_size`).
    pub world_cache_position_base_cell_size: f32,
    /// World cache LOD distance transition scale (`SolariLighting::world_cache_position_lod_scale`).
    pub world_cache_position_lod_scale: f32,
    /// Whether spatiotemporal ReSTIR resampling is enabled (`SolariLighting::restir`).
    pub restir: FfiBool,
    /// Set to `true` to discard temporal history on camera cuts (`SolariLighting::reset`).
    pub reset: FfiBool,
    /// Explicit zero-reserved alignment padding bytes.
    pub reserved0: [u8; 2],
}

#[cfg(feature = "bridge-solari")]
impl Default for FfiSolariLighting {
    fn default() -> Self {
        Self::from_bevy(&SolariLighting::default())
    }
}

#[cfg(feature = "bridge-solari")]
impl FfiSolariLighting {
    /// Projects a Bevy [`SolariLighting`] into [`FfiSolariLighting`].
    pub fn from_bevy(solari: &SolariLighting) -> Self {
        Self {
            confidence_weight_cap: solari.confidence_weight_cap,
            primary_di_samples: solari.primary_di_samples,
            secondary_di_samples: solari.secondary_di_samples,
            max_bounces: solari.max_bounces,
            world_cache_max_temporal_samples: solari.world_cache_max_temporal_samples,
            world_cache_direct_light_sample_count: solari.world_cache_direct_light_sample_count,
            world_cache_max_gi_ray_distance: solari.world_cache_max_gi_ray_distance,
            world_cache_cell_updates_soft_target: solari.world_cache_cell_updates_soft_target,
            world_cache_position_base_cell_size: solari.world_cache_position_base_cell_size,
            world_cache_position_lod_scale: solari.world_cache_position_lod_scale,
            restir: FfiBool::from_bool(solari.restir),
            reset: FfiBool::from_bool(solari.reset),
            reserved0: [0; 2],
        }
    }

    /// Builds a Bevy [`SolariLighting`] component from this mirror.
    pub fn to_bevy(&self) -> SolariLighting {
        let mut solari = SolariLighting::default();
        self.apply_to_bevy(&mut solari);
        solari
    }

    /// Applies mirror fields onto `dst` (ignoring non-finite floats).
    pub fn apply_to_bevy(&self, dst: &mut SolariLighting) {
        dst.restir = self.restir.get();
        if self.confidence_weight_cap.is_finite() {
            dst.confidence_weight_cap = self.confidence_weight_cap;
        }
        dst.primary_di_samples = self.primary_di_samples;
        dst.secondary_di_samples = self.secondary_di_samples;
        dst.max_bounces = self.max_bounces;
        if self.world_cache_max_temporal_samples.is_finite() {
            dst.world_cache_max_temporal_samples = self.world_cache_max_temporal_samples;
        }
        dst.world_cache_direct_light_sample_count = self.world_cache_direct_light_sample_count;
        if self.world_cache_max_gi_ray_distance.is_finite() {
            dst.world_cache_max_gi_ray_distance = self.world_cache_max_gi_ray_distance;
        }
        dst.world_cache_cell_updates_soft_target = self.world_cache_cell_updates_soft_target;
        if self.world_cache_position_base_cell_size.is_finite() {
            dst.world_cache_position_base_cell_size = self.world_cache_position_base_cell_size;
        }
        if self.world_cache_position_lod_scale.is_finite() {
            dst.world_cache_position_lod_scale = self.world_cache_position_lod_scale;
        }
        dst.reset = self.reset.get();
    }
}

#[cfg(feature = "bridge-solari")]
fn spawn_solari_lighting_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiSolariLighting), Without<SolariLighting>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

#[cfg(feature = "bridge-solari")]
fn push_solari_lighting_to_bevy(
    mut query: Query<(&FfiSolariLighting, &mut SolariLighting), Changed<FfiSolariLighting>>,
) {
    for (ffi, mut solari) in &mut query {
        if FfiSolariLighting::from_bevy(&solari) != *ffi {
            ffi.apply_to_bevy(&mut solari);
        }
    }
}

#[cfg(feature = "bridge-solari")]
fn pull_solari_lighting_from_bevy(
    mut query: Query<(&mut FfiSolariLighting, Ref<SolariLighting>), Changed<SolariLighting>>,
) {
    for (mut ffi, solari) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiSolariLighting::from_bevy(&solari);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// Compile-time ABI layout & AbiPod invariant assertions
// ---------------------------------------------------------------------------

const _: () = {
    #[cfg(feature = "bridge-light")]
    {
        assert!(core::mem::size_of::<FfiCamera2d>() == 24);
        assert!(core::mem::align_of::<FfiCamera2d>() == 4);
        assert!(core::mem::size_of::<FfiCamera3d>() == 48);
        assert!(core::mem::align_of::<FfiCamera3d>() == 4);
        assert!(core::mem::size_of::<FfiPointLight>() == 44);
        assert!(core::mem::align_of::<FfiPointLight>() == 4);
        assert!(core::mem::size_of::<FfiSpotLight>() == 52);
        assert!(core::mem::align_of::<FfiSpotLight>() == 4);
        assert!(core::mem::size_of::<FfiDirectionalLight>() == 32);
        assert!(core::mem::align_of::<FfiDirectionalLight>() == 4);
    }
    #[cfg(feature = "bridge-camera-controller")]
    {
        assert!(core::mem::size_of::<FfiPanOrbitCamera>() == 136);
        assert!(core::mem::align_of::<FfiPanOrbitCamera>() == 8);
    }
    #[cfg(feature = "bridge-solari")]
    {
        assert!(core::mem::size_of::<FfiSolariLighting>() == 44);
        assert!(core::mem::align_of::<FfiSolariLighting>() == 4);
    }
};

#[allow(dead_code)]
fn _assert_all_camera_light_wire_types_are_abi_pod() {
    #[cfg(feature = "bridge-light")]
    {
        assert_abi_pod::<FfiCamera2d>();
        assert_abi_pod::<FfiCamera3d>();
        assert_abi_pod::<FfiPointLight>();
        assert_abi_pod::<FfiSpotLight>();
        assert_abi_pod::<FfiDirectionalLight>();
    }
    #[cfg(feature = "bridge-camera-controller")]
    assert_abi_pod::<FfiPanOrbitCamera>();
    #[cfg(feature = "bridge-solari")]
    assert_abi_pod::<FfiSolariLighting>();
}
