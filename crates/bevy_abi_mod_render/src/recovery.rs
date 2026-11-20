//! Bounded GPU device-loss recovery ([`RenderErrorHandler`] + [`RenderErrorPolicy::Recover`]) and
//! [`RenderStartup`] cache invalidation hooks for `bevy_abi_mod_render` (Bevy `0.20`).
//!
//! # Architecture
//!
//! Bevy `0.20` drives rendering through system schedules (`ExtractSchedule`, `Render`,
//! `RenderGraph`, `Core2d`, `Core3d`, and [`RenderStartup`]). When `wgpu` reports
//! [`ErrorType::DeviceLost`], Bevy's renderer state machine invokes [`RenderErrorHandler`].
//! Returning [`RenderErrorPolicy::Recover`] instructs Bevy to recreate `RenderDevice`,
//! `RenderQueue`, and a fresh `PipelineCache` (`needs_shader_reload = true`) and then run
//! [`RenderStartup`] before resuming [`Render`](bevy_render::Render).
//!
//! [`ModRenderRecoveryPlugin`] installs [`mod_render_error_handler`] with bounded retry limits
//! ([`ModRenderRecoveryPolicy`]), structured telemetry ([`ModRenderRecoveryTelemetry`]), extensible
//! recovery hooks ([`ModRenderRecoveryHooks`]), and automatic [`RenderStartup`] invalidation for:
//!
//! - [`ExtractedModMeshPipelines`] cached
//!   mesh-shader [`CachedRenderPipelineId`](bevy_material::descriptor::CachedRenderPipelineId)s
//! - [`bevy_sprite_render::ViewKeyCache`] and
//!   [`bevy_sprite_render::SpecializedMaterial2dPipelineCache`] /
//!   [`SpecializedMeshPipelines`] so existing unchanged 2D mod sprites and materials re-specialize
//!   cleanly after [`RenderStartup`]
//! - [`bevy_pbr::ViewKeyCache`] and [`bevy_pbr::SpecializedMaterialPipelineCache`]

use crate::sprite_mesh_shaders::{
    ExtractedModMeshPipelines, ModExtendedSpriteMaterial2d, ModWeslSpriteExtension2d,
    install_mesh_pipeline_reset_in_sub_app,
};
use crate::sub_app_materials::ModSpriteExtension2d;
use bevy_app::{App, AppExit, AppLabel, Plugin};
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Or, Query, Res, ResMut, Resource, With};
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_ecs::world::World;
use bevy_extract::{Extract, ExtractSchedule};
use bevy_pbr::{
    SpecializedMaterialPipelineCache as PbrSpecializedMaterialPipelineCache,
    ViewKeyCache as PbrViewKeyCache,
};
use bevy_render::camera::{DirtySpecializationSystems, DirtySpecializations};
use bevy_render::error_handler::{ErrorType, RenderError, RenderErrorHandler, RenderErrorPolicy};
use bevy_render::render_resource::SpecializedMeshPipelines;
use bevy_render::settings::{RenderCreation, WgpuSettings};
use bevy_render::sync_world::MainEntity;
use bevy_render::{RenderApp, RenderStartup};
use bevy_sprite_render::{
    ExtendedMaterial2d, Material2dPipelineSpecializer, Mesh2dPipeline, MeshMaterial2d,
    RenderMaterial2dInstances, SpecializedMaterial2dPipelineCache, SpriteMaterial,
    SpriteMeshMaterial, ViewKeyCache as SpriteViewKeyCache,
};
use core::fmt;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// Default upper bound on consecutive [`ErrorType::DeviceLost`] recoveries permitted within
/// [`DEFAULT_RECOVERY_WINDOW`] before [`mod_render_error_handler`] stops rendering.
pub const DEFAULT_MAX_CONSECUTIVE_RECOVERIES: u32 = 3;

/// Default sliding time window used by [`ModRenderRecoveryPolicy`] to bound rapid device-loss
/// recovery loops.
pub const DEFAULT_RECOVERY_WINDOW: Duration = Duration::from_secs(60);

/// Policy controlling bounded GPU device-loss recovery in [`ModRenderRecoveryPlugin`].
///
/// Only [`ErrorType::DeviceLost`] is eligible for automatic [`RenderErrorPolicy::Recover`].
/// Non-device-loss errors (`OutOfMemory`, `Validation`, `Internal`) always stop rendering
/// immediately to avoid hazardous infinite recovery/flashing loops.
#[derive(Resource, Clone)]
pub struct ModRenderRecoveryPolicy {
    /// Maximum number of [`ErrorType::DeviceLost`] recoveries allowed within
    /// [`Self::recovery_window`] (or across the process lifetime when `recovery_window` is `None`).
    /// Setting this to `0` disables automatic recovery.
    pub max_consecutive_recoveries: u32,
    /// Optional sliding window over which recovery attempts are counted toward
    /// [`Self::max_consecutive_recoveries`].
    ///
    /// Isolated device resets separated by more than `recovery_window` age out without tripping the
    /// circuit breaker, whereas rapid repeated losses halt rendering until
    /// [`ModRenderRecoveryTelemetry::reset_budget`] is called explicitly.
    pub recovery_window: Option<Duration>,
    /// [`WgpuSettings`] passed to [`RenderCreation::Automatic`] when recovering from
    /// [`ErrorType::DeviceLost`].
    pub wgpu_settings: WgpuSettings,
    /// If `true`, writes [`AppExit::error()`] once when rendering is stopped due to a
    /// non-recoverable error or exhausted recovery budget. Defaults to `false`.
    pub exit_on_unrecoverable: bool,
    /// If `true` (default), [`register_mod_shader_buffer_in_world`](crate::register_mod_shader_buffer_in_world)
    /// retains [`RenderAssetUsages::MAIN_WORLD`](bevy_asset::RenderAssetUsages::MAIN_WORLD) on
    /// registered [`ShaderBuffer`](bevy_render::storage::ShaderBuffer) assets so Bevy's
    /// `StoragePlugin` can re-upload them after device-loss recovery.
    pub preserve_shader_buffer_cpu_copy: bool,
}

impl Default for ModRenderRecoveryPolicy {
    fn default() -> Self {
        Self {
            max_consecutive_recoveries: DEFAULT_MAX_CONSECUTIVE_RECOVERIES,
            recovery_window: Some(DEFAULT_RECOVERY_WINDOW),
            wgpu_settings: WgpuSettings::default(),
            exit_on_unrecoverable: false,
            preserve_shader_buffer_cpu_copy: true,
        }
    }
}

impl fmt::Debug for ModRenderRecoveryPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModRenderRecoveryPolicy")
            .field(
                "max_consecutive_recoveries",
                &self.max_consecutive_recoveries,
            )
            .field("recovery_window", &self.recovery_window)
            .field("exit_on_unrecoverable", &self.exit_on_unrecoverable)
            .field(
                "preserve_shader_buffer_cpu_copy",
                &self.preserve_shader_buffer_cpu_copy,
            )
            .finish_non_exhaustive()
    }
}

impl ModRenderRecoveryPolicy {
    /// Creates a [`ModRenderRecoveryPolicy`] with default bounded recovery settings (`3` recoveries
    /// per `60s` window, `preserve_shader_buffer_cpu_copy = true`).
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the maximum number of [`ErrorType::DeviceLost`] recoveries permitted before stopping.
    pub fn with_max_consecutive_recoveries(mut self, max: u32) -> Self {
        self.max_consecutive_recoveries = max;
        self
    }

    /// Sets the optional sliding time window for counting recovery attempts.
    ///
    /// Pass `None` for a strict lifetime recovery cap until
    /// [`ModRenderRecoveryTelemetry::reset_budget`] is invoked.
    pub fn with_recovery_window(mut self, window: Option<Duration>) -> Self {
        self.recovery_window = window;
        self
    }

    /// Overrides the [`WgpuSettings`] passed to [`RenderCreation::Automatic`] on recovery.
    pub fn with_wgpu_settings(mut self, settings: WgpuSettings) -> Self {
        self.wgpu_settings = settings;
        self
    }

    /// Configures whether [`AppExit::error()`] is emitted when rendering halts.
    pub fn with_exit_on_unrecoverable(mut self, exit_on_unrecoverable: bool) -> Self {
        self.exit_on_unrecoverable = exit_on_unrecoverable;
        self
    }

    /// Configures whether registered mod [`ShaderBuffer`](bevy_render::storage::ShaderBuffer)
    /// assets automatically retain `RenderAssetUsages::MAIN_WORLD` for device-loss re-upload.
    pub fn with_preserve_shader_buffer_cpu_copy(mut self, preserve: bool) -> Self {
        self.preserve_shader_buffer_cpu_copy = preserve;
        self
    }
}

/// Decision made by [`mod_render_error_handler`] for a [`RenderError`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModRenderRecoveryOutcome {
    /// Initiated bounded renderer re-initialization via [`RenderErrorPolicy::Recover`] for
    /// [`ErrorType::DeviceLost`].
    Recovered,
    /// Returned [`RenderErrorPolicy::StopRendering`] because the error type is not
    /// [`ErrorType::DeviceLost`] (`OutOfMemory`, `Validation`, or `Internal`).
    StoppedNonRecoverableError,
    /// Returned [`RenderErrorPolicy::StopRendering`] because the bounded
    /// [`ErrorType::DeviceLost`] recovery budget was exhausted (or already halted).
    StoppedRecoveryLimitReached,
}

/// Summary of cached GPU pipeline and specialization entries cleared during recovery or
/// [`RenderStartup`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ModRenderCacheInvalidationReport {
    /// Number of active entries cleared from [`ExtractedModMeshPipelines`].
    pub mesh_pipelines_cleared: usize,
    /// Number of view keys cleared from [`bevy_sprite_render::ViewKeyCache`].
    pub sprite_view_keys_cleared: usize,
    /// Number of view entries cleared from [`SpecializedMaterial2dPipelineCache`].
    pub sprite_material_caches_cleared: usize,
    /// Whether `SpecializedMeshPipelines<Material2dPipelineSpecializer>` was reset.
    pub material2d_specializer_reset: bool,
    /// Whether `SpecializedMeshPipelines<Mesh2dPipeline>` was reset.
    pub mesh2d_specializer_reset: bool,
    /// Number of view keys cleared from [`bevy_pbr::ViewKeyCache`].
    pub pbr_view_keys_cleared: usize,
    /// Number of view entries cleared from [`bevy_pbr::SpecializedMaterialPipelineCache`].
    pub pbr_material_caches_cleared: usize,
}

/// Main-world [`Resource`] recording structured telemetry for [`mod_render_error_handler`].
#[derive(Resource, Debug, Clone, Default)]
pub struct ModRenderRecoveryTelemetry {
    recovery_attempts: u32,
    recent_recoveries: VecDeque<Instant>,
    total_handler_invocations: u64,
    stopped_count: u64,
    halted: bool,
    app_exit_sent: bool,
    last_error_type: Option<ErrorType>,
    last_error_description: Option<String>,
    last_outcome: Option<ModRenderRecoveryOutcome>,
    last_invalidation: ModRenderCacheInvalidationReport,
    recovery_hooks_invoked: u64,
}

impl ModRenderRecoveryTelemetry {
    /// Creates an empty [`ModRenderRecoveryTelemetry`] resource.
    pub fn new() -> Self {
        Self::default()
    }

    /// Total number of times [`RenderErrorPolicy::Recover`] has been returned.
    pub fn recovery_attempts(&self) -> u32 {
        self.recovery_attempts
    }

    /// Number of recovery timestamps currently tracked inside the active recovery window.
    pub fn recent_recovery_count(&self) -> usize {
        self.recent_recoveries.len()
    }

    /// Total number of invocations of [`mod_render_error_handler`].
    pub fn total_handler_invocations(&self) -> u64 {
        self.total_handler_invocations
    }

    /// Number of distinct transitions into [`RenderErrorPolicy::StopRendering`].
    ///
    /// Bevy re-polls [`RenderErrorHandler`] each frame while stopped; repeated polls while already
    /// halted do not double-count `stopped_count`.
    pub fn stopped_count(&self) -> u64 {
        self.stopped_count
    }

    /// Returns `true` if rendering has been halted (`StopRendering`) due to a non-recoverable
    /// error or exhausted recovery budget.
    pub fn is_halted(&self) -> bool {
        self.halted
    }

    /// Returns the most recently observed [`ErrorType`], if any.
    pub fn last_error_type(&self) -> Option<ErrorType> {
        self.last_error_type
    }

    /// Returns the most recently observed [`RenderError::description`], if any.
    pub fn last_error_description(&self) -> Option<&str> {
        self.last_error_description.as_deref()
    }

    /// Returns the most recent [`ModRenderRecoveryOutcome`], if any.
    pub fn last_outcome(&self) -> Option<ModRenderRecoveryOutcome> {
        self.last_outcome
    }

    /// Returns the [`ModRenderCacheInvalidationReport`] from the most recent recovery attempt.
    pub fn last_invalidation(&self) -> ModRenderCacheInvalidationReport {
        self.last_invalidation
    }

    /// Total number of custom [`ModRenderRecoveryHooks`] callback executions triggered by recovery.
    pub fn recovery_hooks_invoked(&self) -> u64 {
        self.recovery_hooks_invoked
    }

    /// Resets the sliding recovery window and clears the halted circuit breaker so subsequent
    /// [`ErrorType::DeviceLost`] errors may attempt recovery again.
    ///
    /// Cumulative counters (`recovery_attempts`, `total_handler_invocations`, `stopped_count`,
    /// `recovery_hooks_invoked`) are preserved for diagnostics.
    pub fn reset_budget(&mut self) {
        self.recent_recoveries.clear();
        self.halted = false;
        self.app_exit_sent = false;
    }

    fn prune_window(&mut self, window: Duration, now: Instant) {
        while let Some(&oldest) = self.recent_recoveries.front() {
            if now.saturating_duration_since(oldest) >= window {
                self.recent_recoveries.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Callback invoked when [`mod_render_error_handler`] transitions into
/// [`RenderErrorPolicy::Recover`].
///
/// Receives `(&mut main_world, &mut render_world)` in the same order as Bevy's
/// [`RenderErrorHandler`].
pub type ModRenderRecoveryHookFn = fn(&mut World, &mut World);

/// Main-world [`Resource`] holding keyed callbacks invoked whenever [`mod_render_error_handler`]
/// initiates GPU device-loss recovery.
#[derive(Resource, Default)]
pub struct ModRenderRecoveryHooks {
    hooks: Vec<(&'static str, ModRenderRecoveryHookFn)>,
}

impl fmt::Debug for ModRenderRecoveryHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let keys: Vec<&'static str> = self.hooks.iter().map(|(k, _)| *k).collect();
        f.debug_struct("ModRenderRecoveryHooks")
            .field("keys", &keys)
            .finish()
    }
}

impl ModRenderRecoveryHooks {
    /// Creates an empty [`ModRenderRecoveryHooks`] registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers or replaces a recovery hook identified by `key`.
    pub fn register(&mut self, key: &'static str, hook: ModRenderRecoveryHookFn) {
        if let Some((_, existing)) = self.hooks.iter_mut().find(|(k, _)| *k == key) {
            *existing = hook;
        } else {
            self.hooks.push((key, hook));
        }
    }

    /// Unregisters the recovery hook identified by `key`, returning `true` if it was present.
    pub fn unregister(&mut self, key: &str) -> bool {
        if let Some(pos) = self.hooks.iter().position(|(k, _)| *k == key) {
            self.hooks.remove(pos);
            true
        } else {
            false
        }
    }

    /// Returns the number of registered custom recovery hooks.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Returns `true` if no custom recovery hooks are registered.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }

    /// Returns `true` if a hook with `key` is registered.
    pub fn contains(&self, key: &str) -> bool {
        self.hooks.iter().any(|(k, _)| *k == key)
    }
}

/// Registers a deduplicated GPU device-loss recovery hook inside `world`'s
/// [`ModRenderRecoveryHooks`] resource.
pub fn register_mod_render_recovery_hook_in_world(
    world: &mut World,
    key: &'static str,
    hook: ModRenderRecoveryHookFn,
) {
    if !world.contains_resource::<ModRenderRecoveryHooks>() {
        world.init_resource::<ModRenderRecoveryHooks>();
    }
    world
        .resource_mut::<ModRenderRecoveryHooks>()
        .register(key, hook);
}

/// Render-world [`Resource`] tracking [`RenderStartup`] invalidation passes and one-shot 2D
/// material re-specialization dirtiness.
#[derive(Resource, Debug, Clone, Default)]
pub struct ModRenderRecoveryRenderState {
    render_startup_generations: u64,
    error_handler_invalidations: u64,
    last_invalidation: ModRenderCacheInvalidationReport,
    pending_force_dirty_2d_materials: bool,
    last_requeued_2d_entities: usize,
}

impl ModRenderRecoveryRenderState {
    /// Number of [`RenderStartup`] passes observed by `bevy_abi_mod_render`.
    pub fn render_startup_generations(&self) -> u64 {
        self.render_startup_generations
    }

    /// Number of immediate cache invalidations performed inside [`mod_render_error_handler`].
    pub fn error_handler_invalidations(&self) -> u64 {
        self.error_handler_invalidations
    }

    /// Cache invalidation counts from the most recent [`RenderStartup`] or recovery invalidation.
    pub fn last_invalidation(&self) -> ModRenderCacheInvalidationReport {
        self.last_invalidation
    }

    /// Whether the next [`ExtractSchedule`] pass will force-dirty all active 2D mod material
    /// entities in [`DirtySpecializations`].
    pub fn pending_force_dirty_2d_materials(&self) -> bool {
        self.pending_force_dirty_2d_materials
    }

    /// Number of 2D material entities marked dirty during the most recent post-`RenderStartup`
    /// [`ExtractSchedule`] pass.
    pub fn last_requeued_2d_entities(&self) -> usize {
        self.last_requeued_2d_entities
    }
}

fn clear_sprite_and_pbr_caches_in_render_world(
    render_world: &mut World,
) -> ModRenderCacheInvalidationReport {
    let mut report = ModRenderCacheInvalidationReport::default();

    if let Some(mut view_keys) = render_world.get_resource_mut::<SpriteViewKeyCache>() {
        report.sprite_view_keys_cleared = view_keys.len();
        view_keys.clear();
    }

    if let Some(mut spec_cache) =
        render_world.get_resource_mut::<SpecializedMaterial2dPipelineCache>()
    {
        report.sprite_material_caches_cleared = spec_cache.len();
        spec_cache.clear();
    }

    if let Some(mut specializer) =
        render_world.get_resource_mut::<SpecializedMeshPipelines<Material2dPipelineSpecializer>>()
    {
        *specializer = SpecializedMeshPipelines::default();
        report.material2d_specializer_reset = true;
    }

    if let Some(mut mesh2d_specializer) =
        render_world.get_resource_mut::<SpecializedMeshPipelines<Mesh2dPipeline>>()
    {
        *mesh2d_specializer = SpecializedMeshPipelines::default();
        report.mesh2d_specializer_reset = true;
    }

    if let Some(mut pbr_view_keys) = render_world.get_resource_mut::<PbrViewKeyCache>() {
        report.pbr_view_keys_cleared = pbr_view_keys.len();
        pbr_view_keys.clear();
    }

    if let Some(mut pbr_spec_cache) =
        render_world.get_resource_mut::<PbrSpecializedMaterialPipelineCache>()
    {
        report.pbr_material_caches_cleared = pbr_spec_cache.len();
        pbr_spec_cache.clear();
    }

    report
}

/// Immediately invalidates cached mod mesh-shader pipelines and 2D/3D material specialization
/// caches in `render_world`, scheduling a one-shot 2D material re-dirty pass on the next
/// [`ExtractSchedule`].
///
/// This is invoked automatically when [`mod_render_error_handler`] returns
/// [`RenderErrorPolicy::Recover`]. Unlike [`reset_mod_mesh_pipelines_on_render_startup`](crate::reset_mod_mesh_pipelines_on_render_startup),
/// it clears active cached IDs without incrementing [`ExtractedModMeshPipelines::recovery_epoch`]
/// (which advances once when Bevy subsequently executes [`RenderStartup`]).
pub fn invalidate_mod_render_caches_in_render_world(
    render_world: &mut World,
) -> ModRenderCacheInvalidationReport {
    let mut report = clear_sprite_and_pbr_caches_in_render_world(render_world);

    if let Some(mut extracted) = render_world.get_resource_mut::<ExtractedModMeshPipelines>() {
        report.mesh_pipelines_cleared = extracted.invalidate_for_device_loss();
    }

    if !render_world.contains_resource::<ModRenderRecoveryRenderState>() {
        render_world.init_resource::<ModRenderRecoveryRenderState>();
    }
    let mut state = render_world.resource_mut::<ModRenderRecoveryRenderState>();
    state.error_handler_invalidations = state.error_handler_invalidations.saturating_add(1);
    state.last_invalidation = report;
    state.pending_force_dirty_2d_materials = true;

    report
}

/// [`RenderStartup`] system clearing cached 2D/3D view-key and material specialization caches so
/// existing entities holding mod `.wesl` materials (`SpriteMaterial<ModWeslSpriteExtension2d>`,
/// `MeshMaterial2d<ModExtendedSpriteMaterial2d>`, `ModSpriteExtension2d`, and `StandardMaterial`)
/// re-specialize cleanly onto the fresh [`PipelineCache`](bevy_render::render_resource::PipelineCache).
pub fn reset_mod_sprite_materials_on_render_startup(render_world: &mut World) {
    let report = clear_sprite_and_pbr_caches_in_render_world(render_world);

    if !render_world.contains_resource::<ModRenderRecoveryRenderState>() {
        render_world.init_resource::<ModRenderRecoveryRenderState>();
    }
    let mut state = render_world.resource_mut::<ModRenderRecoveryRenderState>();
    state.render_startup_generations = state.render_startup_generations.saturating_add(1);
    state.last_invalidation = report;
    state.pending_force_dirty_2d_materials = true;

    if report.sprite_view_keys_cleared > 0
        || report.sprite_material_caches_cleared > 0
        || report.pbr_view_keys_cleared > 0
        || report.pbr_material_caches_cleared > 0
    {
        tracing::info!(
            render_startup_generation = state.render_startup_generations,
            sprite_view_keys_cleared = report.sprite_view_keys_cleared,
            sprite_material_caches_cleared = report.sprite_material_caches_cleared,
            pbr_view_keys_cleared = report.pbr_view_keys_cleared,
            pbr_material_caches_cleared = report.pbr_material_caches_cleared,
            "cleared cached mod material specialization state on RenderStartup"
        );
    }
}

type Mod2dMaterialEntityFilter = Or<(
    With<MeshMaterial2d<ModExtendedSpriteMaterial2d>>,
    With<MeshMaterial2d<ExtendedMaterial2d<SpriteMeshMaterial, ModSpriteExtension2d>>>,
    With<SpriteMaterial<ModWeslSpriteExtension2d>>,
    With<SpriteMaterial<ModSpriteExtension2d>>,
)>;

/// [`ExtractSchedule`] system running in [`DirtySpecializationSystems::CheckForChanges`] that
/// marks all active 2D mod material entities dirty once after [`RenderStartup`], ensuring Bevy
/// re-specializes unchanged 2D entities after GPU device-loss recovery or shader reloads.
///
/// On normal frames (`pending_force_dirty_2d_materials == false`), this system returns immediately
/// without iterating queries.
pub fn requeue_mod_2d_materials_after_render_startup(
    state: Option<ResMut<ModRenderRecoveryRenderState>>,
    dirty_specializations: Option<ResMut<DirtySpecializations>>,
    render_material_instances: Option<Res<RenderMaterial2dInstances>>,
    main_mod_2d_entities: Extract<Query<Entity, Mod2dMaterialEntityFilter>>,
) {
    let Some(mut state) = state else {
        return;
    };
    if !state.pending_force_dirty_2d_materials {
        return;
    }
    state.pending_force_dirty_2d_materials = false;

    let Some(mut dirty) = dirty_specializations else {
        state.last_requeued_2d_entities = 0;
        return;
    };

    let mut requeued = 0usize;
    if let Some(instances) = render_material_instances {
        for &main_entity in instances.keys() {
            if dirty.changed_renderables.insert(main_entity) {
                requeued += 1;
            }
        }
    }
    for entity in &main_mod_2d_entities {
        if dirty.changed_renderables.insert(MainEntity::from(entity)) {
            requeued += 1;
        }
    }
    state.last_requeued_2d_entities = requeued;
}

#[derive(Resource, Default)]
struct ModSpriteRecoverySystemsInstalled;

pub(crate) fn install_sprite_material_recovery_in_sub_app<L: AppLabel + Default + Copy + Eq>(
    app: &mut App,
) {
    let Some(sub_app) = app.get_sub_app_mut(L::default()) else {
        return;
    };
    if sub_app
        .world()
        .contains_resource::<ModSpriteRecoverySystemsInstalled>()
    {
        return;
    }
    sub_app
        .init_resource::<ModRenderRecoveryRenderState>()
        .init_resource::<ModSpriteRecoverySystemsInstalled>();

    sub_app.add_systems(RenderStartup, reset_mod_sprite_materials_on_render_startup);
    sub_app.add_systems(
        ExtractSchedule,
        requeue_mod_2d_materials_after_render_startup
            .in_set(DirtySpecializationSystems::CheckForChanges),
    );
}

/// Evaluates [`ModRenderRecoveryPolicy`] for `error` at explicit timestamp `now`, updating
/// [`ModRenderRecoveryTelemetry`], invalidating mod render caches, and invoking
/// [`ModRenderRecoveryHooks`] when returning [`RenderErrorPolicy::Recover`].
pub fn handle_mod_render_error_at(
    error: &RenderError,
    main_world: &mut World,
    render_world: &mut World,
    now: Instant,
) -> RenderErrorPolicy {
    if !main_world.contains_resource::<ModRenderRecoveryTelemetry>() {
        main_world.init_resource::<ModRenderRecoveryTelemetry>();
    }
    let policy = main_world
        .get_resource::<ModRenderRecoveryPolicy>()
        .cloned()
        .unwrap_or_default();

    let (should_recover, emit_exit) = {
        let mut telemetry = main_world.resource_mut::<ModRenderRecoveryTelemetry>();
        telemetry.total_handler_invocations = telemetry.total_handler_invocations.saturating_add(1);
        telemetry.last_error_type = Some(error.ty);
        telemetry.last_error_description = Some(error.description.clone());

        if error.ty != ErrorType::DeviceLost {
            let newly_halted = !telemetry.halted;
            telemetry.halted = true;
            telemetry.last_outcome = Some(ModRenderRecoveryOutcome::StoppedNonRecoverableError);
            if newly_halted {
                telemetry.stopped_count = telemetry.stopped_count.saturating_add(1);
                tracing::error!(
                    error_type = ?error.ty,
                    description = %error.description,
                    "stopping rendering on non-DeviceLost RenderError (automatic recovery disabled for non-DeviceLost errors)"
                );
            }
            let should_exit = policy.exit_on_unrecoverable && !telemetry.app_exit_sent;
            if should_exit {
                telemetry.app_exit_sent = true;
            }
            (false, should_exit)
        } else {
            if let Some(window) = policy.recovery_window {
                telemetry.prune_window(window, now);
            }

            let budget_available = !telemetry.halted
                && policy.max_consecutive_recoveries > 0
                && telemetry.recent_recoveries.len() < policy.max_consecutive_recoveries as usize;

            if !budget_available {
                let newly_halted = !telemetry.halted;
                telemetry.halted = true;
                telemetry.last_outcome =
                    Some(ModRenderRecoveryOutcome::StoppedRecoveryLimitReached);
                if newly_halted {
                    telemetry.stopped_count = telemetry.stopped_count.saturating_add(1);
                    tracing::error!(
                        recovery_attempts = telemetry.recovery_attempts,
                        max_consecutive_recoveries = policy.max_consecutive_recoveries,
                        description = %error.description,
                        "DeviceLost recovery budget exhausted; stopping rendering to prevent infinite recovery loop"
                    );
                }
                let should_exit = policy.exit_on_unrecoverable && !telemetry.app_exit_sent;
                if should_exit {
                    telemetry.app_exit_sent = true;
                }
                (false, should_exit)
            } else {
                telemetry.recovery_attempts = telemetry.recovery_attempts.saturating_add(1);
                telemetry.recent_recoveries.push_back(now);
                telemetry.last_outcome = Some(ModRenderRecoveryOutcome::Recovered);
                tracing::warn!(
                    attempt = telemetry.recovery_attempts,
                    recent_in_window = telemetry.recent_recoveries.len(),
                    max_consecutive_recoveries = policy.max_consecutive_recoveries,
                    description = %error.description,
                    "DeviceLost detected; initiating bounded renderer recovery"
                );
                (true, false)
            }
        }
    };

    if emit_exit {
        let _ = main_world.write_message(AppExit::error());
    }

    if !should_recover {
        return RenderErrorPolicy::StopRendering;
    }

    let report = invalidate_mod_render_caches_in_render_world(render_world);

    let hooks: Vec<(&'static str, ModRenderRecoveryHookFn)> = main_world
        .get_resource::<ModRenderRecoveryHooks>()
        .map(|h| h.hooks.clone())
        .unwrap_or_default();
    let hooks_count = hooks.len() as u64;
    for (_, hook) in hooks {
        hook(main_world, render_world);
    }

    if let Some(mut telemetry) = main_world.get_resource_mut::<ModRenderRecoveryTelemetry>() {
        telemetry.last_invalidation = report;
        telemetry.recovery_hooks_invoked =
            telemetry.recovery_hooks_invoked.saturating_add(hooks_count);
    }

    RenderErrorPolicy::Recover(RenderCreation::Automatic(Box::new(policy.wgpu_settings)))
}

/// Bevy `0.20` [`RenderErrorHandler`] function installed by [`ModRenderRecoveryPlugin`].
///
/// Delegates to [`handle_mod_render_error_at`] with [`Instant::now()`].
pub fn mod_render_error_handler(
    error: &RenderError,
    main_world: &mut World,
    render_world: &mut World,
) -> RenderErrorPolicy {
    handle_mod_render_error_at(error, main_world, render_world, Instant::now())
}

/// Opt-in Bevy [`Plugin`] installing bounded GPU device-loss recovery
/// ([`RenderErrorHandler`] + [`RenderErrorPolicy::Recover`]) and [`RenderStartup`] cache
/// invalidation systems for mod shaders, 2D/3D materials, and mesh-shader pipelines.
///
/// Works regardless of whether it is added before or after Bevy's `RenderPlugin` or
/// [`ModSubAppPlugin<RenderApp>`](crate::ModSubAppPlugin).
#[derive(Clone, Debug, Default)]
pub struct ModRenderRecoveryPlugin {
    policy: ModRenderRecoveryPolicy,
}

impl ModRenderRecoveryPlugin {
    /// Creates a [`ModRenderRecoveryPlugin`] with an explicit [`ModRenderRecoveryPolicy`].
    pub fn new(policy: ModRenderRecoveryPolicy) -> Self {
        Self { policy }
    }

    /// Convenience constructor setting [`ModRenderRecoveryPolicy::max_consecutive_recoveries`].
    pub fn with_max_recoveries(max_consecutive_recoveries: u32) -> Self {
        Self::new(
            ModRenderRecoveryPolicy::default()
                .with_max_consecutive_recoveries(max_consecutive_recoveries),
        )
    }

    /// Returns the configured [`ModRenderRecoveryPolicy`].
    pub fn policy(&self) -> &ModRenderRecoveryPolicy {
        &self.policy
    }
}

impl Plugin for ModRenderRecoveryPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(self.policy.clone())
            .insert_resource(RenderErrorHandler(mod_render_error_handler));
        if !app
            .world()
            .contains_resource::<ModRenderRecoveryTelemetry>()
        {
            app.init_resource::<ModRenderRecoveryTelemetry>();
        }
        if !app.world().contains_resource::<ModRenderRecoveryHooks>() {
            app.init_resource::<ModRenderRecoveryHooks>();
        }

        install_sprite_material_recovery_in_sub_app::<RenderApp>(app);
        install_mesh_pipeline_reset_in_sub_app::<RenderApp>(app);
    }

    fn finish(&self, app: &mut App) {
        install_sprite_material_recovery_in_sub_app::<RenderApp>(app);
        install_mesh_pipeline_reset_in_sub_app::<RenderApp>(app);
    }
}

/// Extension trait on [`World`] for registering device-loss recovery hooks and inspecting or
/// resetting [`ModRenderRecoveryTelemetry`].
pub trait ModRenderRecoveryWorldExt {
    /// Registers or replaces a keyed callback invoked whenever [`mod_render_error_handler`]
    /// triggers [`RenderErrorPolicy::Recover`].
    fn register_mod_render_recovery_hook(
        &mut self,
        key: &'static str,
        hook: ModRenderRecoveryHookFn,
    );

    /// Unregisters a recovery hook by `key`, returning `true` if it was present.
    fn unregister_mod_render_recovery_hook(&mut self, key: &str) -> bool;

    /// Returns a reference to [`ModRenderRecoveryTelemetry`], if initialized.
    fn mod_render_recovery_telemetry(&self) -> Option<&ModRenderRecoveryTelemetry>;

    /// Resets the bounded recovery budget and halted circuit breaker, returning `true` if
    /// [`ModRenderRecoveryTelemetry`] was present.
    fn reset_mod_render_recovery_budget(&mut self) -> bool;
}

impl ModRenderRecoveryWorldExt for World {
    #[inline]
    fn register_mod_render_recovery_hook(
        &mut self,
        key: &'static str,
        hook: ModRenderRecoveryHookFn,
    ) {
        register_mod_render_recovery_hook_in_world(self, key, hook);
    }

    #[inline]
    fn unregister_mod_render_recovery_hook(&mut self, key: &str) -> bool {
        self.get_resource_mut::<ModRenderRecoveryHooks>()
            .is_some_and(|mut hooks| hooks.unregister(key))
    }

    #[inline]
    fn mod_render_recovery_telemetry(&self) -> Option<&ModRenderRecoveryTelemetry> {
        self.get_resource::<ModRenderRecoveryTelemetry>()
    }

    #[inline]
    fn reset_mod_render_recovery_budget(&mut self) -> bool {
        if let Some(mut telemetry) = self.get_resource_mut::<ModRenderRecoveryTelemetry>() {
            telemetry.reset_budget();
            true
        } else {
            false
        }
    }
}

/// Extension trait on [`App`] for registering device-loss recovery hooks and inspecting or
/// resetting [`ModRenderRecoveryTelemetry`].
pub trait ModRenderRecoveryAppExt {
    /// Registers or replaces a keyed callback invoked whenever [`mod_render_error_handler`]
    /// triggers [`RenderErrorPolicy::Recover`].
    fn register_mod_render_recovery_hook(
        &mut self,
        key: &'static str,
        hook: ModRenderRecoveryHookFn,
    ) -> &mut Self;

    /// Unregisters a recovery hook by `key`, returning `true` if it was present.
    fn unregister_mod_render_recovery_hook(&mut self, key: &str) -> bool;

    /// Returns a reference to [`ModRenderRecoveryTelemetry`], if initialized.
    fn mod_render_recovery_telemetry(&self) -> Option<&ModRenderRecoveryTelemetry>;

    /// Resets the bounded recovery budget and halted circuit breaker, returning `true` if
    /// [`ModRenderRecoveryTelemetry`] was present.
    fn reset_mod_render_recovery_budget(&mut self) -> bool;
}

impl ModRenderRecoveryAppExt for App {
    #[inline]
    fn register_mod_render_recovery_hook(
        &mut self,
        key: &'static str,
        hook: ModRenderRecoveryHookFn,
    ) -> &mut Self {
        self.world_mut()
            .register_mod_render_recovery_hook(key, hook);
        self
    }

    #[inline]
    fn unregister_mod_render_recovery_hook(&mut self, key: &str) -> bool {
        self.world_mut().unregister_mod_render_recovery_hook(key)
    }

    #[inline]
    fn mod_render_recovery_telemetry(&self) -> Option<&ModRenderRecoveryTelemetry> {
        self.world().mod_render_recovery_telemetry()
    }

    #[inline]
    fn reset_mod_render_recovery_budget(&mut self) -> bool {
        self.world_mut().reset_mod_render_recovery_budget()
    }
}
