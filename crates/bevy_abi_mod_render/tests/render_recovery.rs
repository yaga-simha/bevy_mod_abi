//! Integration tests for bounded GPU device-loss recovery (`ModRenderRecoveryPlugin`,
//! `ModRenderRecoveryPolicy`, `ModRenderRecoveryTelemetry`, `ModRenderRecoveryHooks`,
//! `RenderErrorHandler`, `RenderErrorPolicy::Recover`) and `ShaderBuffer` recovery retention.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::LoadOptions;
use bevy_abi_mod_render::prelude::*;
use bevy_abi_mod_render::{handle_mod_render_error_at, mod_render_error_handler};
use bevy_app::{App, AppExit};
use bevy_asset::{Assets, RenderAssetUsages};
use bevy_ecs::entity::Entity;
use bevy_ecs::message::Messages;
use bevy_ecs::world::World;
use bevy_material::descriptor::CachedRenderPipelineId;
use bevy_render::error_handler::{ErrorType, RenderError, RenderErrorHandler, RenderErrorPolicy};
use bevy_render::settings::RenderCreation;
use bevy_render::storage::ShaderBuffer;
use bevy_render::sync_world::MainEntity;
use bevy_render::view::RetainedViewEntity;
use bevy_render::{RenderApp, RenderStartup};
use bevy_sprite_render::{
    Mesh2dPipelineKey, SpecializedMaterial2dPipelineCache, ViewKeyCache as SpriteViewKeyCache,
};
use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ directory")
        .parent()
        .expect("workspace root directory")
        .to_path_buf()
}

fn fixture_dylib(stem: &str) -> PathBuf {
    let dir = std::env::var_os("BAM_FIXTURE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("CARGO_TARGET_DIR")
                .filter(|v| !v.is_empty())
                .map(|d| PathBuf::from(d).join("fixtures"))
        })
        .unwrap_or_else(|| workspace_root().join("target").join("fixtures"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let path = dir.join(format!("{prefix}{stem}.{ext}"));
    if !path.is_file() {
        panic!(
            "Fixture missing at {} — run 'cargo xtask fixtures' before 'cargo test'",
            path.display()
        );
    }
    path
}

struct TempDylibCopy {
    path: PathBuf,
    dir: PathBuf,
}

impl TempDylibCopy {
    fn new(original: &Path, tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let ext = original
            .extension()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "dylib".to_owned());
        let dir = std::env::temp_dir().join(format!(
            "bevy_abi_mod_render_recovery_{}_{}",
            std::process::id(),
            id
        ));
        fs::create_dir_all(&dir).expect("create shadow dir");
        let path = dir.join(format!("{tag}_{id}.{ext}"));
        fs::copy(original, &path).expect("copy shadow dylib");
        Self { path, dir }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDylibCopy {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(&self.dir);
    }
}

fn device_lost_error(msg: &str) -> RenderError {
    RenderError {
        ty: ErrorType::DeviceLost,
        description: msg.to_owned(),
        source: None,
    }
}

fn invoke_installed_handler(app: &mut App, error: &RenderError) -> RenderErrorPolicy {
    let handler_fn = app.world().resource::<RenderErrorHandler>().0;
    let mut dummy_render_world = World::new();
    handler_fn(error, app.world_mut(), &mut dummy_render_world)
}

#[test]
fn device_lost_recovers_up_to_cap_then_stops_safely_and_resets_cleanly() {
    static HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn count_hook(_main: &mut World, _render: &mut World) {
        HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    HOOK_CALLS.store(0, Ordering::Relaxed);

    let mut app = App::new();
    app.add_plugins(ModRenderRecoveryPlugin::with_max_recoveries(3));
    app.register_mod_render_recovery_hook("test::count_hook", count_hook);
    // Registering with the same key must deduplicate rather than run twice.
    app.register_mod_render_recovery_hook("test::count_hook", count_hook);

    assert_eq!(app.world().resource::<ModRenderRecoveryHooks>().len(), 1);

    for attempt in 1..=3u32 {
        let policy = invoke_installed_handler(&mut app, &device_lost_error("GPU reset"));
        assert!(
            matches!(
                policy,
                RenderErrorPolicy::Recover(RenderCreation::Automatic(_))
            ),
            "attempt {attempt} must return RenderErrorPolicy::Recover"
        );
        let telemetry = app
            .mod_render_recovery_telemetry()
            .expect("telemetry present");
        assert_eq!(telemetry.recovery_attempts(), attempt);
        assert_eq!(telemetry.recent_recovery_count(), attempt as usize);
        assert_eq!(telemetry.recovery_hooks_invoked(), u64::from(attempt));
        assert!(!telemetry.is_halted());
        assert_eq!(telemetry.stopped_count(), 0);
        assert_eq!(telemetry.last_error_type(), Some(ErrorType::DeviceLost));
        assert_eq!(telemetry.last_error_description(), Some("GPU reset"));
        assert_eq!(
            telemetry.last_outcome(),
            Some(ModRenderRecoveryOutcome::Recovered)
        );
    }
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 3);

    // Attempt 4 exceeds the cap of 3 and must stop rendering safely without running hooks.
    let fourth = invoke_installed_handler(&mut app, &device_lost_error("GPU loop"));
    assert!(matches!(fourth, RenderErrorPolicy::StopRendering));
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 3);

    // Subsequent per-frame polls while stopped must remain StopRendering without double-counting
    // stopped transitions or triggering hooks.
    let fifth = invoke_installed_handler(&mut app, &device_lost_error("GPU loop"));
    assert!(matches!(fifth, RenderErrorPolicy::StopRendering));
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 3);

    {
        let telemetry = app.mod_render_recovery_telemetry().unwrap();
        assert_eq!(telemetry.recovery_attempts(), 3);
        assert!(telemetry.is_halted());
        assert_eq!(telemetry.stopped_count(), 1);
        assert_eq!(telemetry.total_handler_invocations(), 5);
        assert_eq!(
            telemetry.last_outcome(),
            Some(ModRenderRecoveryOutcome::StoppedRecoveryLimitReached)
        );
    }

    // Explicit host reset re-arms bounded recovery while preserving cumulative counters.
    assert!(app.reset_mod_render_recovery_budget());
    assert!(app.unregister_mod_render_recovery_hook("test::count_hook"));
    assert!(!app.unregister_mod_render_recovery_hook("test::count_hook"));

    let post_reset = invoke_installed_handler(&mut app, &device_lost_error("manual retry"));
    assert!(matches!(
        post_reset,
        RenderErrorPolicy::Recover(RenderCreation::Automatic(_))
    ));
    let telemetry = app.mod_render_recovery_telemetry().unwrap();
    assert_eq!(telemetry.recovery_attempts(), 4);
    assert_eq!(telemetry.recent_recovery_count(), 1);
    assert!(!telemetry.is_halted());
    assert_eq!(HOOK_CALLS.load(Ordering::Relaxed), 3);
}

#[test]
fn non_device_lost_errors_never_blindly_recover() {
    static NON_DEVICE_LOST_HOOK_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn should_not_run(_main: &mut World, _render: &mut World) {
        NON_DEVICE_LOST_HOOK_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    NON_DEVICE_LOST_HOOK_CALLS.store(0, Ordering::Relaxed);

    for (err_ty, desc) in [
        (ErrorType::OutOfMemory, "GPU OOM"),
        (ErrorType::Validation, "invalid bind group layout"),
        (ErrorType::Internal, "backend internal fault"),
    ] {
        let mut app = App::new();
        app.add_plugins(ModRenderRecoveryPlugin::new(
            ModRenderRecoveryPolicy::default()
                .with_max_consecutive_recoveries(5)
                .with_exit_on_unrecoverable(true),
        ));
        app.register_mod_render_recovery_hook("test::never_run", should_not_run);

        let err = RenderError {
            ty: err_ty,
            description: desc.to_owned(),
            source: None,
        };

        // First poll halts rendering and emits AppExit::error() once.
        let p1 = invoke_installed_handler(&mut app, &err);
        assert!(
            matches!(p1, RenderErrorPolicy::StopRendering),
            "{err_ty:?} must not trigger RenderErrorPolicy::Recover"
        );
        // Second poll while halted remains StopRendering without duplicate AppExit or stopped_count.
        let p2 = invoke_installed_handler(&mut app, &err);
        assert!(matches!(p2, RenderErrorPolicy::StopRendering));

        let telemetry = app.mod_render_recovery_telemetry().unwrap();
        assert_eq!(telemetry.recovery_attempts(), 0);
        assert_eq!(telemetry.recovery_hooks_invoked(), 0);
        assert!(telemetry.is_halted());
        assert_eq!(telemetry.stopped_count(), 1);
        assert_eq!(telemetry.total_handler_invocations(), 2);
        assert_eq!(telemetry.last_error_type(), Some(err_ty));
        assert_eq!(telemetry.last_error_description(), Some(desc));
        assert_eq!(
            telemetry.last_outcome(),
            Some(ModRenderRecoveryOutcome::StoppedNonRecoverableError)
        );

        let exit_messages = app.world().resource::<Messages<AppExit>>();
        assert_eq!(
            exit_messages.len(),
            1,
            "AppExit::error() must be emitted at most once per halt for {err_ty:?}"
        );
    }

    assert_eq!(NON_DEVICE_LOST_HOOK_CALLS.load(Ordering::Relaxed), 0);
}

#[test]
fn zero_recovery_budget_stops_immediately_on_device_lost() {
    let mut app = App::new();
    app.add_plugins(ModRenderRecoveryPlugin::with_max_recoveries(0));

    let policy = invoke_installed_handler(&mut app, &device_lost_error("recovery disabled"));
    assert!(matches!(policy, RenderErrorPolicy::StopRendering));

    let telemetry = app.mod_render_recovery_telemetry().unwrap();
    assert_eq!(telemetry.recovery_attempts(), 0);
    assert!(telemetry.is_halted());
    assert_eq!(telemetry.stopped_count(), 1);
    assert_eq!(
        telemetry.last_outcome(),
        Some(ModRenderRecoveryOutcome::StoppedRecoveryLimitReached)
    );
}

#[test]
fn sliding_recovery_window_allows_isolated_resets_and_stays_halted_once_tripped() {
    let mut main_world = World::new();
    let mut render_world = World::new();
    main_world.insert_resource(
        ModRenderRecoveryPolicy::new()
            .with_max_consecutive_recoveries(2)
            .with_recovery_window(Some(Duration::from_secs(30))),
    );
    main_world.init_resource::<ModRenderRecoveryTelemetry>();

    let t0 = Instant::now();
    let err = device_lost_error("driver timeout");

    // 1st and 2nd recoveries inside window succeed.
    assert!(matches!(
        handle_mod_render_error_at(&err, &mut main_world, &mut render_world, t0),
        RenderErrorPolicy::Recover(_)
    ));
    assert!(matches!(
        handle_mod_render_error_at(
            &err,
            &mut main_world,
            &mut render_world,
            t0 + Duration::from_secs(10),
        ),
        RenderErrorPolicy::Recover(_)
    ));

    // After t0 expires (>= 30s later), an isolated reset recovers cleanly.
    assert!(matches!(
        handle_mod_render_error_at(
            &err,
            &mut main_world,
            &mut render_world,
            t0 + Duration::from_secs(35),
        ),
        RenderErrorPolicy::Recover(_)
    ));
    assert_eq!(
        main_world
            .mod_render_recovery_telemetry()
            .unwrap()
            .recovery_attempts(),
        3
    );

    // Rapid follow-up within window trips the circuit breaker (t0+10s and t0+35s both in window).
    assert!(matches!(
        handle_mod_render_error_at(
            &err,
            &mut main_world,
            &mut render_world,
            t0 + Duration::from_secs(36),
        ),
        RenderErrorPolicy::StopRendering
    ));
    assert!(
        main_world
            .mod_render_recovery_telemetry()
            .unwrap()
            .is_halted()
    );

    // Even after the window elapses, a halted circuit breaker MUST NOT spontaneously resume until
    // the host explicitly calls `reset_mod_render_recovery_budget()`.
    assert!(matches!(
        handle_mod_render_error_at(
            &err,
            &mut main_world,
            &mut render_world,
            t0 + Duration::from_secs(180),
        ),
        RenderErrorPolicy::StopRendering
    ));
    assert_eq!(
        main_world
            .mod_render_recovery_telemetry()
            .unwrap()
            .recovery_attempts(),
        3
    );
}

#[test]
fn error_handler_recovery_and_render_startup_coordinate_single_epoch_and_clear_caches() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "recovery_e2e");

    let mut app = App::new();
    app.add_plugins((
        AbiModPlugin,
        ModSubAppPlugin::<RenderApp>::default(),
        ModWeslMaterialPlugin,
        ModSpriteMaterialPlugin::default(),
        ModMeshShaderPlugin::default(),
        ModRenderRecoveryPlugin::with_max_recoveries(2),
    ));

    let mod_id = app
        .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
        .expect("load mod");
    let mesh_shader = app
        .register_mod_wesl_shader(
            mod_id,
            WeslShaderAssetSpec::from_wesl(
                "mesh_recovery",
                "mods/recovery/mesh.wesl",
                "@mesh fn mesh_main() {}",
            ),
        )
        .expect("register mesh shader");
    let pipe_id = app
        .register_mod_mesh_pipeline(
            mod_id,
            ModMeshPipelineSpec::new("recovery_pipe", mesh_shader),
        )
        .expect("register mesh pipeline");

    // Extract pipeline into RenderApp.
    app.update();

    // Seed synthetic 2D specialization cache entries to verify error-handler invalidation clears them.
    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        assert!(
            render_world
                .resource::<ExtractedModMeshPipelines>()
                .contains(pipe_id)
        );
        let mut view_keys = SpriteViewKeyCache::default();
        view_keys.insert(
            MainEntity::from(Entity::PLACEHOLDER),
            Mesh2dPipelineKey::NONE,
        );
        render_world.insert_resource(view_keys);

        let mut spec_cache = SpecializedMaterial2dPipelineCache::default();
        let view_entity = RetainedViewEntity::new(MainEntity::from(Entity::PLACEHOLDER), None, 0);
        spec_cache.entry(view_entity).or_default().insert(
            MainEntity::from(Entity::PLACEHOLDER),
            CachedRenderPipelineId::new(7),
        );
        render_world.insert_resource(spec_cache);
    }

    // Invoke `mod_render_error_handler` with both real worlds (`MainWorld` + `RenderApp`).
    let policy = {
        let bevy_app::SubApps { main, sub_apps } = app.sub_apps_mut();
        let render_sub = sub_apps
            .get_mut(&bevy_app::AppLabel::intern(&RenderApp))
            .expect("RenderApp present");
        mod_render_error_handler(
            &device_lost_error("simulated driver TDR"),
            main.world_mut(),
            render_sub.world_mut(),
        )
    };
    assert!(matches!(
        policy,
        RenderErrorPolicy::Recover(RenderCreation::Automatic(_))
    ));

    // Immediately after error handler: cached pipelines & 2D caches are already cleared, while
    // `recovery_epoch` stays at 0 until Bevy executes `RenderStartup` on the recreated device.
    {
        let telemetry = app.mod_render_recovery_telemetry().unwrap();
        assert_eq!(telemetry.recovery_attempts(), 1);
        assert_eq!(telemetry.last_invalidation().mesh_pipelines_cleared, 1);
        assert_eq!(telemetry.last_invalidation().sprite_view_keys_cleared, 1);
        assert_eq!(
            telemetry.last_invalidation().sprite_material_caches_cleared,
            1
        );

        let render_world = app.get_sub_app(RenderApp).unwrap().world();
        let extracted = render_world.resource::<ExtractedModMeshPipelines>();
        assert!(extracted.is_empty());
        assert_eq!(extracted.recovery_epoch(), 0);
        assert!(render_world.resource::<SpriteViewKeyCache>().is_empty());
        assert!(
            render_world
                .resource::<SpecializedMaterial2dPipelineCache>()
                .is_empty()
        );
    }

    // Simulate Bevy running `RenderStartup` once the recreated device unpacks.
    {
        let render_world = app.get_sub_app_mut(RenderApp).unwrap().world_mut();
        render_world.run_schedule(RenderStartup);

        let extracted = render_world.resource::<ExtractedModMeshPipelines>();
        assert_eq!(
            extracted.recovery_epoch(),
            1,
            "recovery_epoch must advance exactly once per RenderStartup"
        );
        assert_eq!(
            extracted.last_reset_cleared_count(),
            1,
            "last_reset_cleared_count must reflect pipelines invalidated for this recovery"
        );
        assert!(extracted.evicted_pipeline_ids().is_empty());
        assert!(extracted.evicted_shader_ids().is_empty());
        assert_eq!(
            render_world
                .resource::<ModRenderRecoveryRenderState>()
                .render_startup_generations(),
            1
        );
    }

    // Next ExtractSchedule re-extracts all live mod mesh pipelines cleanly.
    app.update();
    {
        let render_world = app.get_sub_app(RenderApp).unwrap().world();
        let extracted = render_world.resource::<ExtractedModMeshPipelines>();
        assert_eq!(extracted.len(), 1);
        assert!(extracted.contains(pipe_id));
        assert_eq!(extracted.recovery_epoch(), 1);
    }

    app.unload_mod(mod_id).expect("unload mod");
}

#[test]
fn recovery_policy_preserves_main_world_on_render_world_only_shader_buffers() {
    let dylib = TempDylibCopy::new(&fixture_dylib("ordered_sets_mod"), "shader_buf_usage");

    let spec = ModShaderBufferSpec::from_f32_slice("render_only_lut", 16, &[1.0, 2.0])
        .with_asset_usage(RenderAssetUsages::RENDER_WORLD);
    assert!(!spec.retains_main_world_for_recovery());
    assert!(
        spec.clone()
            .with_device_loss_recovery()
            .retains_main_world_for_recovery()
    );

    // 1. Default ModRenderRecoveryPolicy preserves MAIN_WORLD so buffers survive device loss.
    {
        let mut app = App::new();
        app.add_plugins((
            AbiModPlugin,
            ModWeslMaterialPlugin,
            ModRenderRecoveryPlugin::default(),
        ));
        let mod_id = app
            .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
            .expect("load mod");
        let handle = app
            .register_mod_shader_buffer(mod_id, spec.clone())
            .expect("register ShaderBuffer");
        let stored = app
            .world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&handle)
            .expect("buffer stored");
        assert!(
            stored.asset_usage.contains(RenderAssetUsages::MAIN_WORLD),
            "preserve_shader_buffer_cpu_copy=true must retain MAIN_WORLD for device-loss recovery"
        );
        assert!(stored.asset_usage.contains(RenderAssetUsages::RENDER_WORLD));
        app.unload_mod(mod_id).expect("unload mod");
    }

    // 2. Opting out via `with_preserve_shader_buffer_cpu_copy(false)` keeps RENDER_WORLD verbatim.
    {
        let mut app = App::new();
        app.add_plugins((
            AbiModPlugin,
            ModWeslMaterialPlugin,
            ModRenderRecoveryPlugin::new(
                ModRenderRecoveryPolicy::default().with_preserve_shader_buffer_cpu_copy(false),
            ),
        ));
        let mod_id = app
            .load_mod_with(dylib.path(), LoadOptions::new().with_shadow_copy(true))
            .expect("load mod");
        let handle = app
            .register_mod_shader_buffer(mod_id, spec)
            .expect("register ShaderBuffer");
        let stored = app
            .world()
            .resource::<Assets<ShaderBuffer>>()
            .get(&handle)
            .expect("buffer stored");
        assert_eq!(stored.asset_usage, RenderAssetUsages::RENDER_WORLD);
        app.unload_mod(mod_id).expect("unload mod");
    }
}

#[test]
fn recovery_plugin_overrides_default_render_error_handler() {
    let mut app = App::new();
    // Simulate `RenderPlugin::build` installing default quit-on-error handler first.
    app.init_resource::<RenderErrorHandler>();
    app.add_plugins(ModRenderRecoveryPlugin::default());

    let policy = invoke_installed_handler(&mut app, &device_lost_error("override check"));
    assert!(matches!(
        policy,
        RenderErrorPolicy::Recover(RenderCreation::Automatic(_))
    ));
    assert!(app.world().resource::<Messages<AppExit>>().is_empty());
}
