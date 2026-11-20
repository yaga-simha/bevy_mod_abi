//! Spatial and non-spatial `bevy_audio` (`0.20`) FFI bridges (`ModAudioBridgePlugin`).
//!
//! Exposes zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror components and
//! bidirectional Bevy ECS sync systems under `#![forbid(unsafe_code)]`:
//!
//! - [`FfiAudioPlaybackSettings`]: Bidirectional mirror of [`PlaybackSettings`] (`PlaybackMode`,
//!   linear/dB [`Volume`], speed, paused, muted, spatial flag, optional [`SpatialScale`],
//!   optional `start_position`, and optional `duration`).
//! - [`FfiSpatialAudioSinkControl`]: Runtime playback control and state readback synced with
//!   [`SpatialAudioSink`] and [`AudioSink`] (play, pause, stop, toggle playback, mute/unmute,
//!   volume, speed, spatial emitter position, and spatial ear positions).
//! - [`FfiAudioPlayerSpec`]: Asset-handle-backed [`AudioPlayer<AudioSource>`] +
//!   [`PlaybackSettings`] specification that spawns/updates audio playback directly on the mod's
//!   entity so unloading the mod automatically despawns its `ModScoped` audio entities and drops
//!   active sinks cleanly.

use crate::asset::ModAssetWorldExt;
use bevy_abi_mod_host::ModWorldExt;
use bevy_abi_mod_host_sys::{ComponentCollisionError, ComponentOptions, ModRegistry};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{BamComponent, ComponentUuid, FfiAssetHandle, FfiBool, assert_abi_pod};
use bevy_app::{App, Plugin, PostUpdate};
use bevy_asset::uuid::Uuid;
use bevy_asset::{AssetApp, AssetId, AssetIndex, AssetServer, Assets, Handle};
use bevy_audio::{
    AudioPlayer, AudioSink, AudioSinkPlayback, AudioSource, PlaybackMode, PlaybackSettings,
    SpatialAudioSink, SpatialScale, Volume,
};
use bevy_ecs::change_detection::{DetectChanges, DetectChangesMut};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::error::{ErrorContext, error as log_ecs_error};
use bevy_ecs::query::{Changed, Has, Or, With, Without};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::system::{Commands, Query, Res, ResMut};
use bevy_ecs::world::{Ref, World};
use bevy_math::Vec3;
use core::marker::PhantomData;
use core::time::Duration;

// ---------------------------------------------------------------------------
// Canonical names & wire discriminants
// ---------------------------------------------------------------------------

/// Canonical ABI name for [`FfiAudioPlaybackSettings`].
pub const CANONICAL_AUDIO_PLAYBACK_SETTINGS_NAME: &str =
    "bevy_abi_mod::audio::FfiAudioPlaybackSettings";
/// Canonical ABI name for [`FfiSpatialAudioSinkControl`].
pub const CANONICAL_SPATIAL_AUDIO_SINK_CONTROL_NAME: &str =
    "bevy_abi_mod::audio::FfiSpatialAudioSinkControl";
/// Canonical ABI name for [`FfiAudioPlayerSpec`].
pub const CANONICAL_AUDIO_PLAYER_SPEC_NAME: &str = "bevy_abi_mod::audio::FfiAudioPlayerSpec";
/// Short alias used when registering [`AudioSource`] in [`ModRegistry`].
pub const AUDIO_SOURCE_ASSET_ALIAS: &str = "AudioSource";

/// `PlaybackMode::Once` discriminant (`0`, default).
pub const PLAYBACK_MODE_ONCE: u8 = 0;
/// `PlaybackMode::Loop` discriminant (`1`).
pub const PLAYBACK_MODE_LOOP: u8 = 1;
/// `PlaybackMode::Despawn` discriminant (`2`).
pub const PLAYBACK_MODE_DESPAWN: u8 = 2;
/// `PlaybackMode::Remove` discriminant (`3`).
pub const PLAYBACK_MODE_REMOVE: u8 = 3;

/// `Volume::Linear` scale discriminant (`0`, default).
pub const VOLUME_LINEAR: u8 = 0;
/// `Volume::Decibels` scale discriminant (`1`).
pub const VOLUME_DECIBELS: u8 = 1;

/// No playback state transition requested (`0`).
pub const SINK_CMD_NONE: u8 = 0;
/// Resume playback (`AudioSinkPlayback::play`, `1`).
pub const SINK_CMD_PLAY: u8 = 1;
/// Pause playback (`AudioSinkPlayback::pause`, `2`).
pub const SINK_CMD_PAUSE: u8 = 2;
/// Permanently stop playback (`AudioSinkPlayback::stop`, `3`).
pub const SINK_CMD_STOP: u8 = 3;
/// Toggle paused/playing state (`AudioSinkPlayback::toggle_playback`, `4`).
pub const SINK_CMD_TOGGLE_PLAYBACK: u8 = 4;

/// No mute transition requested (`0`).
pub const MUTE_CMD_NONE: u8 = 0;
/// Mute sink (`AudioSinkPlayback::mute`, `1`).
pub const MUTE_CMD_MUTE: u8 = 1;
/// Unmute sink (`AudioSinkPlayback::unmute`, `2`).
pub const MUTE_CMD_UNMUTE: u8 = 2;
/// Toggle muted state (`AudioSinkPlayback::toggle_mute`, `3`).
pub const MUTE_CMD_TOGGLE: u8 = 3;

/// Apply `FfiSpatialAudioSinkControl::volume` (per `volume_mode`) to the sink (`1 << 0`).
pub const APPLY_VOLUME: u8 = 1 << 0;
/// Apply `FfiSpatialAudioSinkControl::speed` to the sink (`1 << 1`).
pub const APPLY_SPEED: u8 = 1 << 1;
/// Apply `FfiSpatialAudioSinkControl::emitter_position` to [`SpatialAudioSink`] (`1 << 2`).
pub const APPLY_EMITTER_POS: u8 = 1 << 2;
/// Apply `left_ear_position` and `right_ear_position` to [`SpatialAudioSink`] (`1 << 3`).
pub const APPLY_EAR_POSITIONS: u8 = 1 << 3;

// ---------------------------------------------------------------------------
// SystemSet & Plugin
// ---------------------------------------------------------------------------

/// Ordered [`SystemSet`] phases in [`PostUpdate`] for bidirectional audio bridge sync.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AudioBridgeSystems {
    /// Spawns missing Bevy audio components and pushes changed FFI specs/controls into Bevy
    /// [`AudioPlayer`], [`PlaybackSettings`], [`SpatialAudioSink`], and [`AudioSink`].
    Push,
    /// Pulls live host-side [`PlaybackSettings`], [`SpatialAudioSink`], and [`AudioSink`] state
    /// back into paired FFI mirror components.
    Pull,
}

/// Bevy [`Plugin`] exposing [`FfiAudioPlaybackSettings`], [`FfiSpatialAudioSinkControl`],
/// [`FfiAudioPlayerSpec`], and the [`AudioSource`] asset type to native mods, and scheduling
/// bidirectional [`PostUpdate`] audio sync systems (`#![forbid(unsafe_code)]`).
#[derive(Default, Debug, Clone, Copy)]
pub struct ModAudioBridgePlugin;

impl Plugin for ModAudioBridgePlugin {
    fn build(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>()
            && !app.world().contains_resource::<Assets<AudioSource>>()
        {
            app.init_asset::<AudioSource>();
        }
        if let Err(err) = register_audio_bridges(app.world_mut()) {
            log_ecs_error(
                err.into(),
                ErrorContext::Command {
                    name: "ModAudioBridgePlugin::build".into(),
                },
            );
        }

        app.configure_sets(
            PostUpdate,
            AudioBridgeSystems::Push.before(AudioBridgeSystems::Pull),
        );
        app.add_systems(
            PostUpdate,
            (
                (
                    spawn_playback_settings_from_ffi,
                    push_playback_settings_to_bevy,
                    push_audio_player_spec_to_bevy,
                    push_audio_sink_control_to_bevy,
                )
                    .in_set(AudioBridgeSystems::Push),
                (
                    pull_playback_settings_from_bevy,
                    pull_audio_player_spec_from_bevy,
                    pull_audio_sink_state_from_bevy,
                )
                    .in_set(AudioBridgeSystems::Pull),
            ),
        );
    }

    fn finish(&self, app: &mut App) {
        if app.world().contains_resource::<AssetServer>()
            && !app.world().contains_resource::<Assets<AudioSource>>()
        {
            app.init_asset::<AudioSource>();
        }
        let _ = register_audio_bridges(app.world_mut());
    }
}

/// Registers [`FfiAudioPlaybackSettings`], [`FfiSpatialAudioSinkControl`],
/// [`FfiAudioPlayerSpec`], and the [`AudioSource`] asset type in `world`'s [`ModRegistry`]
/// idempotently.
pub fn register_audio_bridges(world: &mut World) -> Result<(), ComponentCollisionError> {
    if !world.contains_resource::<ModRegistry>() {
        world.insert_resource(ModRegistry::new());
    }
    if !world.contains_resource::<Assets<AudioSource>>() {
        world.insert_resource(Assets::<AudioSource>::default());
    }

    register_mirror_if_missing::<FfiAudioPlaybackSettings>(world)?;
    register_mirror_if_missing::<FfiSpatialAudioSinkControl>(world)?;
    register_mirror_if_missing::<FfiAudioPlayerSpec>(world)?;

    if world
        .resource::<ModRegistry>()
        .get_by_name(core::any::type_name::<AudioSource>())
        .is_none()
    {
        world.register_mod_asset_with_alias::<AudioSource>(Some(AUDIO_SOURCE_ASSET_ALIAS))?;
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
// Conversion helpers
// ---------------------------------------------------------------------------

#[inline]
fn playback_mode_to_tag(mode: PlaybackMode) -> u8 {
    match mode {
        PlaybackMode::Once => PLAYBACK_MODE_ONCE,
        PlaybackMode::Loop => PLAYBACK_MODE_LOOP,
        PlaybackMode::Despawn => PLAYBACK_MODE_DESPAWN,
        PlaybackMode::Remove => PLAYBACK_MODE_REMOVE,
    }
}

#[inline]
fn tag_to_playback_mode(tag: u8) -> PlaybackMode {
    match tag {
        PLAYBACK_MODE_LOOP => PlaybackMode::Loop,
        PLAYBACK_MODE_DESPAWN => PlaybackMode::Despawn,
        PLAYBACK_MODE_REMOVE => PlaybackMode::Remove,
        _ => PlaybackMode::Once,
    }
}

#[inline]
fn volume_to_ffi(volume: Volume) -> (u8, f32) {
    match volume {
        Volume::Linear(v) => (VOLUME_LINEAR, v),
        Volume::Decibels(db) => (VOLUME_DECIBELS, db),
    }
}

#[inline]
fn ffi_to_volume(mode: u8, value: f32) -> Volume {
    if mode == VOLUME_DECIBELS {
        if value.is_nan() {
            Volume::Decibels(0.0)
        } else {
            Volume::Decibels(value)
        }
    } else if value.is_finite() {
        Volume::Linear(value.abs())
    } else {
        Volume::Linear(1.0)
    }
}

#[inline]
fn secs_to_duration_opt(enabled: FfiBool, secs: f32) -> Option<Duration> {
    if enabled.get() && secs.is_finite() && secs >= 0.0 {
        Some(Duration::from_secs_f32(secs))
    } else {
        None
    }
}

/// Converts a Bevy [`Handle<AudioSource>`] into its C-ABI [`FfiAssetHandle`] wire value using
/// the registered `audio_source_type` [`BamComponent`] handle.
pub fn audio_source_handle_to_ffi(
    audio_source_type: BamComponent,
    handle: &Handle<AudioSource>,
) -> FfiAssetHandle {
    match handle.id() {
        AssetId::Index { index, .. } => {
            FfiAssetHandle::from_index_bits(audio_source_type, index.to_bits())
        }
        AssetId::Uuid { uuid } => {
            FfiAssetHandle::from_uuid(audio_source_type, ComponentUuid::from_u128(uuid.as_u128()))
        }
    }
}

/// Resolves an [`FfiAssetHandle`] into a strong (or UUID) Bevy [`Handle<AudioSource>`].
///
/// Rejects handles that fail [`FfiAssetHandle::validate`] or carry an `asset_type` mismatched
/// with `expected_audio_type` (when `Some`).
pub fn resolve_audio_source_handle(
    handle: FfiAssetHandle,
    expected_audio_type: Option<BamComponent>,
    asset_server: Option<&AssetServer>,
    audio_assets: Option<&mut Assets<AudioSource>>,
) -> Option<Handle<AudioSource>> {
    if !handle.is_some() || !handle.validate().is_ok() {
        return None;
    }
    if let Some(expected) = expected_audio_type
        && handle.asset_type != expected
    {
        return None;
    }
    match handle.kind {
        FfiAssetHandle::KIND_UUID => Some(Handle::Uuid(
            Uuid::from_u128(handle.uuid.to_u128()),
            PhantomData,
        )),
        FfiAssetHandle::KIND_INDEX => {
            let bits = handle.to_index_bits()?;
            let id = AssetId::<AudioSource>::from(AssetIndex::from_bits(bits));
            if let Some(server) = asset_server
                && let Some(strong) = server.get_id_handle::<AudioSource>(id)
            {
                return Some(strong);
            }
            audio_assets.and_then(|assets| assets.get_strong_handle(id))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// FfiAudioPlaybackSettings
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` mirror of Bevy [`PlaybackSettings`].
///
/// Layout: `36` bytes, `4`-byte alignment, zero implicit compiler padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::audio::FfiAudioPlaybackSettings")]
pub struct FfiAudioPlaybackSettings {
    /// Volume magnitude (`Volume::Linear` when `volume_mode == VOLUME_LINEAR`, or
    /// `Volume::Decibels` when `volume_mode == VOLUME_DECIBELS`).
    pub volume: f32,
    /// Playback speed multiplier (`1.0` = normal speed).
    pub speed: f32,
    /// Custom spatial scale `[x, y, z]` applied when `has_spatial_scale` is true.
    pub spatial_scale: [f32; 3],
    /// Clip start offset in seconds when `has_start_position` is true.
    pub start_position_secs: f32,
    /// Maximum playback duration in seconds when `has_duration` is true.
    pub duration_secs: f32,
    /// Playback completion mode (`PLAYBACK_MODE_ONCE..=PLAYBACK_MODE_REMOVE`).
    pub mode: u8,
    /// Volume scale discriminant (`VOLUME_LINEAR` or `VOLUME_DECIBELS`).
    pub volume_mode: u8,
    /// Whether playback starts paused.
    pub paused: FfiBool,
    /// Whether playback starts muted.
    pub muted: FfiBool,
    /// Whether spatial 3D panning is enabled (`SpatialAudioSink`).
    pub spatial: FfiBool,
    /// Whether `spatial_scale` overrides `DefaultSpatialScale`.
    pub has_spatial_scale: FfiBool,
    /// Whether `start_position_secs` is active (`Some(Duration)`).
    pub has_start_position: FfiBool,
    /// Whether `duration_secs` is active (`Some(Duration)`).
    pub has_duration: FfiBool,
}

impl Default for FfiAudioPlaybackSettings {
    fn default() -> Self {
        Self::from_bevy(&PlaybackSettings::ONCE)
    }
}

impl FfiAudioPlaybackSettings {
    /// Projects Bevy [`PlaybackSettings`] into [`FfiAudioPlaybackSettings`].
    pub fn from_bevy(settings: &PlaybackSettings) -> Self {
        let (volume_mode, volume) = volume_to_ffi(settings.volume);
        let (has_spatial_scale, spatial_scale) = match settings.spatial_scale {
            Some(scale) => (FfiBool::TRUE, scale.0.to_array()),
            None => (FfiBool::FALSE, Vec3::ONE.to_array()),
        };
        let (has_start_position, start_position_secs) = match settings.start_position {
            Some(d) => (FfiBool::TRUE, d.as_secs_f32()),
            None => (FfiBool::FALSE, 0.0),
        };
        let (has_duration, duration_secs) = match settings.duration {
            Some(d) => (FfiBool::TRUE, d.as_secs_f32()),
            None => (FfiBool::FALSE, 0.0),
        };
        Self {
            volume,
            speed: settings.speed,
            spatial_scale,
            start_position_secs,
            duration_secs,
            mode: playback_mode_to_tag(settings.mode),
            volume_mode,
            paused: FfiBool::from_bool(settings.paused),
            muted: FfiBool::from_bool(settings.muted),
            spatial: FfiBool::from_bool(settings.spatial),
            has_spatial_scale,
            has_start_position,
            has_duration,
        }
    }

    /// Converts this mirror into Bevy [`PlaybackSettings`].
    pub fn to_bevy(&self) -> PlaybackSettings {
        let spatial_scale =
            if self.has_spatial_scale.get() && self.spatial_scale.iter().all(|v| v.is_finite()) {
                Some(SpatialScale(Vec3::from_array(self.spatial_scale)))
            } else {
                None
            };
        PlaybackSettings {
            mode: tag_to_playback_mode(self.mode),
            volume: ffi_to_volume(self.volume_mode, self.volume),
            speed: if self.speed.is_finite() && self.speed > 0.0 {
                self.speed
            } else {
                1.0
            },
            paused: self.paused.get(),
            muted: self.muted.get(),
            spatial: self.spatial.get(),
            spatial_scale,
            start_position: secs_to_duration_opt(self.has_start_position, self.start_position_secs),
            duration: secs_to_duration_opt(self.has_duration, self.duration_secs),
        }
    }
}

fn spawn_playback_settings_from_ffi(
    mut commands: Commands,
    query: Query<(Entity, &FfiAudioPlaybackSettings), Without<PlaybackSettings>>,
) {
    for (entity, ffi) in &query {
        commands.entity(entity).insert(ffi.to_bevy());
    }
}

fn push_playback_settings_to_bevy(
    mut query: Query<
        (&FfiAudioPlaybackSettings, &mut PlaybackSettings),
        Changed<FfiAudioPlaybackSettings>,
    >,
) {
    for (ffi, mut settings) in &mut query {
        if FfiAudioPlaybackSettings::from_bevy(&settings) != *ffi {
            *settings = ffi.to_bevy();
        }
    }
}

fn pull_playback_settings_from_bevy(
    mut query: Query<
        (&mut FfiAudioPlaybackSettings, Ref<PlaybackSettings>),
        Changed<PlaybackSettings>,
    >,
) {
    for (mut ffi, settings) in &mut query {
        if ffi.is_changed() {
            continue;
        }
        let projected = FfiAudioPlaybackSettings::from_bevy(&settings);
        if *ffi != projected {
            *ffi.bypass_change_detection() = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiAudioPlayerSpec
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` specification spawning and updating
/// [`AudioPlayer<AudioSource>`] + [`PlaybackSettings`] on the owning entity.
///
/// When spawned by a guest mod on a `ModScoped(mod_id)` entity, unloading the mod despawns the
/// entity and drops its [`AudioPlayer<AudioSource>`] and active [`AudioSink`] /
/// [`SpatialAudioSink`] automatically.
///
/// Layout: `68` bytes, `4`-byte alignment, zero implicit compiler padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, Default, ModComponent)]
#[mod_component(name = "bevy_abi_mod::audio::FfiAudioPlayerSpec")]
pub struct FfiAudioPlayerSpec {
    /// Handle to the [`AudioSource`] asset (`KIND_INDEX`, `KIND_UUID`, or `KIND_NONE`).
    pub source: FfiAssetHandle,
    /// Playback configuration mirrored into [`PlaybackSettings`].
    pub settings: FfiAudioPlaybackSettings,
}

impl FfiAudioPlayerSpec {
    /// Creates an [`FfiAudioPlayerSpec`] for `source` with the given `settings`.
    #[inline]
    pub const fn new(source: FfiAssetHandle, settings: FfiAudioPlaybackSettings) -> Self {
        Self { source, settings }
    }
}

type AudioPlayerSpecPushQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Ref<'static, FfiAudioPlayerSpec>,
        Option<&'static mut AudioPlayer<AudioSource>>,
        Option<&'static mut PlaybackSettings>,
        Has<FfiAudioPlaybackSettings>,
    ),
>;

fn push_audio_player_spec_to_bevy(
    mut commands: Commands,
    registry: Option<Res<ModRegistry>>,
    asset_server: Option<Res<AssetServer>>,
    mut audio_assets: Option<ResMut<Assets<AudioSource>>>,
    mut query: AudioPlayerSpecPushQuery<'_, '_>,
) {
    let expected_audio_type = registry
        .as_deref()
        .and_then(|reg| reg.get_by_name(core::any::type_name::<AudioSource>()))
        .map(|entry| entry.handle);

    for (entity, spec, existing_player, existing_settings, has_standalone_settings) in &mut query {
        let needs_attach = existing_player.is_none() && spec.source.is_some();
        if !spec.is_changed() && !needs_attach {
            continue;
        }

        if spec.source.is_none() {
            if existing_player.is_some() {
                commands.entity(entity).remove::<AudioPlayer<AudioSource>>();
            }
            continue;
        }

        let Some(handle) = resolve_audio_source_handle(
            spec.source,
            expected_audio_type,
            asset_server.as_deref(),
            audio_assets.as_deref_mut(),
        ) else {
            continue;
        };

        let desired_settings = spec.settings.to_bevy();
        match existing_player {
            None => {
                let mut ec = commands.entity(entity);
                if has_standalone_settings || existing_settings.is_some() {
                    ec.insert(AudioPlayer::new(handle));
                    if !has_standalone_settings
                        && let Some(mut settings) = existing_settings
                        && FfiAudioPlaybackSettings::from_bevy(&settings) != spec.settings
                    {
                        *settings = desired_settings;
                    }
                } else {
                    ec.insert((AudioPlayer::new(handle), desired_settings));
                }
            }
            Some(mut player) => {
                if player.0.id() != handle.id() {
                    player.0 = handle;
                }
                if !has_standalone_settings {
                    match existing_settings {
                        Some(mut settings) => {
                            if FfiAudioPlaybackSettings::from_bevy(&settings) != spec.settings {
                                *settings = desired_settings;
                            }
                        }
                        None => {
                            commands.entity(entity).insert(desired_settings);
                        }
                    }
                }
            }
        }
    }
}

fn pull_audio_player_spec_from_bevy(
    mut query: Query<
        (&mut FfiAudioPlayerSpec, Ref<PlaybackSettings>),
        (Changed<PlaybackSettings>, Without<FfiAudioPlaybackSettings>),
    >,
) {
    for (mut spec, settings) in &mut query {
        if spec.is_changed() {
            continue;
        }
        let projected = FfiAudioPlaybackSettings::from_bevy(&settings);
        if spec.settings != projected {
            spec.bypass_change_detection().settings = projected;
        }
    }
}

// ---------------------------------------------------------------------------
// FfiSpatialAudioSinkControl
// ---------------------------------------------------------------------------

/// Zero-padding `#[repr(C)]` `AbiPod` + `ModComponent` runtime control and telemetry mirror for
/// Bevy's [`SpatialAudioSink`] and [`AudioSink`] components.
///
/// Guests issue one-shot commands via `playback_command`, `mute_command`, and `apply_flags`
/// (`APPLY_VOLUME`, `APPLY_SPEED`, `APPLY_EMITTER_POS`, `APPLY_EAR_POSITIONS`). Once applied to
/// an attached sink, command fields are automatically cleared back to `0` and live sink state
/// (`volume`, `speed`, `position_secs`, `is_paused`, `is_muted`, `is_empty`, `is_spatial`) is
/// synced back for guest inspection.
///
/// Layout: `56` bytes, `4`-byte alignment, zero implicit compiler padding.
#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bevy_abi_mod::audio::FfiSpatialAudioSinkControl")]
pub struct FfiSpatialAudioSinkControl {
    /// Target (when `APPLY_VOLUME` is set) or observed sink volume (linear or dB per `volume_mode`).
    pub volume: f32,
    /// Target (when `APPLY_SPEED` is set) or observed sink playback speed (`1.0` = normal).
    pub speed: f32,
    /// Observed sink playback position in seconds (`AudioSinkPlayback::position`).
    pub position_secs: f32,
    /// Spatial emitter position `[x, y, z]` applied when `APPLY_EMITTER_POS` is set.
    pub emitter_position: [f32; 3],
    /// Spatial listener left-ear position `[x, y, z]` applied when `APPLY_EAR_POSITIONS` is set.
    pub left_ear_position: [f32; 3],
    /// Spatial listener right-ear position `[x, y, z]` applied when `APPLY_EAR_POSITIONS` is set.
    pub right_ear_position: [f32; 3],
    /// One-shot playback command (`SINK_CMD_NONE..=SINK_CMD_TOGGLE_PLAYBACK`).
    pub playback_command: u8,
    /// One-shot mute command (`MUTE_CMD_NONE..=MUTE_CMD_TOGGLE`).
    pub mute_command: u8,
    /// Volume scale discriminant (`VOLUME_LINEAR` or `VOLUME_DECIBELS`).
    pub volume_mode: u8,
    /// Bitmask of fields to push (`APPLY_VOLUME | APPLY_SPEED | APPLY_EMITTER_POS | APPLY_EAR_POSITIONS`).
    pub apply_flags: u8,
    /// Observed paused state (`AudioSinkPlayback::is_paused`).
    pub is_paused: FfiBool,
    /// Observed muted state (`AudioSinkPlayback::is_muted`).
    pub is_muted: FfiBool,
    /// Observed empty/drained state (`AudioSinkPlayback::empty`).
    pub is_empty: FfiBool,
    /// `true` when synchronized against [`SpatialAudioSink`], `false` for [`AudioSink`] or detached.
    pub is_spatial: FfiBool,
}

impl Default for FfiSpatialAudioSinkControl {
    fn default() -> Self {
        Self {
            volume: 1.0,
            speed: 1.0,
            position_secs: 0.0,
            emitter_position: [0.0, 0.0, 0.0],
            left_ear_position: [-2.0, 0.0, 0.0],
            right_ear_position: [2.0, 0.0, 0.0],
            playback_command: SINK_CMD_NONE,
            mute_command: MUTE_CMD_NONE,
            volume_mode: VOLUME_LINEAR,
            apply_flags: 0,
            is_paused: FfiBool::FALSE,
            is_muted: FfiBool::FALSE,
            is_empty: FfiBool::FALSE,
            is_spatial: FfiBool::FALSE,
        }
    }
}

impl FfiSpatialAudioSinkControl {
    /// Returns `true` if any one-shot command or apply flag is pending.
    #[inline]
    pub const fn has_pending_command(&self) -> bool {
        self.playback_command != SINK_CMD_NONE
            || self.mute_command != MUTE_CMD_NONE
            || self.apply_flags != 0
    }

    /// Queues a [`SINK_CMD_PLAY`] command.
    #[inline]
    pub fn request_play(&mut self) {
        self.playback_command = SINK_CMD_PLAY;
    }

    /// Queues a [`SINK_CMD_PAUSE`] command.
    #[inline]
    pub fn request_pause(&mut self) {
        self.playback_command = SINK_CMD_PAUSE;
    }

    /// Queues a [`SINK_CMD_STOP`] command.
    #[inline]
    pub fn request_stop(&mut self) {
        self.playback_command = SINK_CMD_STOP;
    }

    /// Queues a [`SINK_CMD_TOGGLE_PLAYBACK`] command.
    #[inline]
    pub fn request_toggle_playback(&mut self) {
        self.playback_command = SINK_CMD_TOGGLE_PLAYBACK;
    }

    /// Queues a [`MUTE_CMD_MUTE`] command.
    #[inline]
    pub fn request_mute(&mut self) {
        self.mute_command = MUTE_CMD_MUTE;
    }

    /// Queues a [`MUTE_CMD_UNMUTE`] command.
    #[inline]
    pub fn request_unmute(&mut self) {
        self.mute_command = MUTE_CMD_UNMUTE;
    }

    /// Queues a [`MUTE_CMD_TOGGLE`] command.
    #[inline]
    pub fn request_toggle_mute(&mut self) {
        self.mute_command = MUTE_CMD_TOGGLE;
    }

    /// Queues a linear volume update (`APPLY_VOLUME`, `VOLUME_LINEAR`).
    #[inline]
    pub fn set_linear_volume(&mut self, linear: f32) {
        self.volume_mode = VOLUME_LINEAR;
        self.volume = linear;
        self.apply_flags |= APPLY_VOLUME;
    }

    /// Queues a decibel volume update (`APPLY_VOLUME`, `VOLUME_DECIBELS`).
    #[inline]
    pub fn set_decibel_volume(&mut self, decibels: f32) {
        self.volume_mode = VOLUME_DECIBELS;
        self.volume = decibels;
        self.apply_flags |= APPLY_VOLUME;
    }

    /// Queues a playback speed update (`APPLY_SPEED`).
    #[inline]
    pub fn set_speed(&mut self, speed: f32) {
        self.speed = speed;
        self.apply_flags |= APPLY_SPEED;
    }

    /// Queues a spatial emitter position update (`APPLY_EMITTER_POS`).
    #[inline]
    pub fn set_emitter_position(&mut self, position: [f32; 3]) {
        self.emitter_position = position;
        self.apply_flags |= APPLY_EMITTER_POS;
    }

    /// Queues a spatial ear positions update (`APPLY_EAR_POSITIONS`).
    #[inline]
    pub fn set_ear_positions(&mut self, left: [f32; 3], right: [f32; 3]) {
        self.left_ear_position = left;
        self.right_ear_position = right;
        self.apply_flags |= APPLY_EAR_POSITIONS;
    }
}

fn apply_common_sink_commands<S: AudioSinkPlayback>(
    control: &FfiSpatialAudioSinkControl,
    sink: &mut S,
) {
    if (control.apply_flags & APPLY_VOLUME) != 0 {
        sink.set_volume(ffi_to_volume(control.volume_mode, control.volume));
    }
    if (control.apply_flags & APPLY_SPEED) != 0 && control.speed.is_finite() && control.speed > 0.0
    {
        sink.set_speed(control.speed);
    }
    match control.mute_command {
        MUTE_CMD_MUTE => sink.mute(),
        MUTE_CMD_UNMUTE => sink.unmute(),
        MUTE_CMD_TOGGLE => sink.toggle_mute(),
        _ => {}
    }
    match control.playback_command {
        SINK_CMD_PLAY => sink.play(),
        SINK_CMD_PAUSE => sink.pause(),
        SINK_CMD_STOP => sink.stop(),
        SINK_CMD_TOGGLE_PLAYBACK => sink.toggle_playback(),
        _ => {}
    }
}

fn snapshot_sink_readback<S: AudioSinkPlayback>(
    sink: &S,
    volume_mode: u8,
    is_spatial: bool,
) -> (f32, f32, f32, FfiBool, FfiBool, FfiBool, FfiBool) {
    let volume = if volume_mode == VOLUME_DECIBELS {
        sink.volume().to_decibels()
    } else {
        sink.volume().to_linear()
    };
    (
        volume,
        sink.speed(),
        sink.position().as_secs_f32(),
        FfiBool::from_bool(sink.is_paused()),
        FfiBool::from_bool(sink.is_muted()),
        FfiBool::from_bool(sink.empty()),
        FfiBool::from_bool(is_spatial),
    )
}

type AudioSinkPushQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut FfiSpatialAudioSinkControl,
        Option<&'static mut SpatialAudioSink>,
        Option<&'static mut AudioSink>,
    ),
    Or<(With<SpatialAudioSink>, With<AudioSink>)>,
>;

fn push_audio_sink_control_to_bevy(mut query: AudioSinkPushQuery<'_, '_>) {
    for (mut control, spatial_sink, regular_sink) in &mut query {
        if !control.has_pending_command() {
            continue;
        }
        let snapshot = *control;
        let readback = if let Some(mut spatial) = spatial_sink {
            apply_common_sink_commands(&snapshot, spatial.as_mut());
            if (snapshot.apply_flags & APPLY_EMITTER_POS) != 0
                && snapshot.emitter_position.iter().all(|v| v.is_finite())
            {
                spatial.set_emitter_position(Vec3::from_array(snapshot.emitter_position));
            }
            if (snapshot.apply_flags & APPLY_EAR_POSITIONS) != 0
                && snapshot.left_ear_position.iter().all(|v| v.is_finite())
                && snapshot.right_ear_position.iter().all(|v| v.is_finite())
            {
                spatial.set_ears_position(
                    Vec3::from_array(snapshot.left_ear_position),
                    Vec3::from_array(snapshot.right_ear_position),
                );
            }
            Some(snapshot_sink_readback(
                spatial.as_ref(),
                snapshot.volume_mode,
                true,
            ))
        } else if let Some(mut sink) = regular_sink {
            apply_common_sink_commands(&snapshot, sink.as_mut());
            Some(snapshot_sink_readback(
                sink.as_ref(),
                snapshot.volume_mode,
                false,
            ))
        } else {
            None
        };

        if let Some((volume, speed, position_secs, is_paused, is_muted, is_empty, is_spatial)) =
            readback
        {
            control.playback_command = SINK_CMD_NONE;
            control.mute_command = MUTE_CMD_NONE;
            control.apply_flags = 0;
            control.volume = volume;
            control.speed = speed;
            control.position_secs = position_secs;
            control.is_paused = is_paused;
            control.is_muted = is_muted;
            control.is_empty = is_empty;
            control.is_spatial = is_spatial;
        }
    }
}

type AudioSinkPullQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut FfiSpatialAudioSinkControl,
        Option<&'static SpatialAudioSink>,
        Option<&'static AudioSink>,
    ),
    Or<(With<SpatialAudioSink>, With<AudioSink>)>,
>;

fn pull_audio_sink_state_from_bevy(mut query: AudioSinkPullQuery<'_, '_>) {
    for (mut control, spatial_sink, regular_sink) in &mut query {
        let readback = if let Some(spatial) = spatial_sink {
            snapshot_sink_readback(spatial, control.volume_mode, true)
        } else if let Some(sink) = regular_sink {
            snapshot_sink_readback(sink, control.volume_mode, false)
        } else {
            continue;
        };
        let (volume, speed, position_secs, is_paused, is_muted, is_empty, is_spatial) = readback;
        if control.volume != volume
            || control.speed != speed
            || control.position_secs != position_secs
            || control.is_paused != is_paused
            || control.is_muted != is_muted
            || control.is_empty != is_empty
            || control.is_spatial != is_spatial
        {
            let ctrl = control.bypass_change_detection();
            ctrl.volume = volume;
            ctrl.speed = speed;
            ctrl.position_secs = position_secs;
            ctrl.is_paused = is_paused;
            ctrl.is_muted = is_muted;
            ctrl.is_empty = is_empty;
            ctrl.is_spatial = is_spatial;
        }
    }
}

// ---------------------------------------------------------------------------
// Compile-time ABI layout & AbiPod invariant assertions
// ---------------------------------------------------------------------------

const _: () = {
    assert!(core::mem::size_of::<FfiAudioPlaybackSettings>() == 36);
    assert!(core::mem::align_of::<FfiAudioPlaybackSettings>() == 4);
    assert!(core::mem::size_of::<FfiAudioPlayerSpec>() == 68);
    assert!(core::mem::align_of::<FfiAudioPlayerSpec>() == 4);
    assert!(core::mem::size_of::<FfiSpatialAudioSinkControl>() == 56);
    assert!(core::mem::align_of::<FfiSpatialAudioSinkControl>() == 4);
};

#[allow(dead_code)]
fn _assert_all_audio_wire_types_are_abi_pod() {
    assert_abi_pod::<FfiAudioPlaybackSettings>();
    assert_abi_pod::<FfiAudioPlayerSpec>();
    assert_abi_pod::<FfiSpatialAudioSinkControl>();
}
