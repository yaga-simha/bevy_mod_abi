#![forbid(unsafe_code)]

//! Safe (`#![forbid(unsafe_code)]`) Cross-Bridge End-to-End Integration Suite (`e2e`).
//!
//! Compiles real temporary `.cdylib` guest crates using `bevy_abi_mod_guest` (`derive` +
//! `GuestPlugin` + `export_mod!` + `bridges`) and exercises a multi-frame Bevy `0.20` simulation
//! combining:
//! - `StdTypesBridgePlugin` + `TransformPlugin` (`Transform`, `GlobalTransform` propagation across
//!   `FfiChildOf`, `Visibility`, `Name`, `Time<Virtual|Fixed|Real>`, `ButtonInput<KeyCode|MouseButton>`)
//! - `ModAssetPlugin` isolated `mod://` namespaces across two concurrently loaded mods (`alpha` and
//!   `beta`), cross-namespace & traversal rejection, async `LoadState::Loaded`, and handle release
//!   upon `unload_mod`
//! - `ModStateExt` (`expose_mod_state`) driving `Menu -> Playing -> Paused -> Menu` with
//!   `set_state_if_different` idempotency, `OnEnter` / `OnExit` / `OnTransition` schedules,
//!   `run_if_in_state`, and `DespawnOnExit` / `DespawnOnEnter` (including `Disabled` entities)

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bevy_app::{App, TaskPoolPlugin};
#[cfg(feature = "bridge-reflect")]
use bevy_asset::AssetApp;
use bevy_asset::AssetPlugin;
use bevy_camera::visibility::Visibility;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::name::Name;
use bevy_ecs::prelude::{Component, Resource};
use bevy_ecs::query::Has;
use bevy_input::ButtonInput;
use bevy_input::keyboard::KeyCode;
use bevy_input::mouse::MouseButton;
use bevy_mod_abi::bridges::ModBridgesPlugin;
use bevy_mod_abi::bridges::asset::ModAssetAppExt;
#[cfg(feature = "bridge-reflect")]
use bevy_mod_abi::bridges::asset::ModAssetRegistry;
use bevy_mod_abi::bridges::state::ModStateExt;
use bevy_mod_abi::prelude::*;
use bevy_state::state::{State, States};
use bevy_state::state_scoped::{DespawnOnEnter, DespawnOnExit};
use bevy_time::{Fixed, Real, Time, Virtual};
use bevy_transform::TransformPlugin;
use bevy_transform::components::{GlobalTransform, Transform};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/ directory")
        .parent()
        .expect("workspace root directory")
        .to_path_buf()
}

fn dylib_filename(stem: &str) -> String {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    format!("{prefix}{stem}.{ext}")
}

// ============================================================================
// Shared Host <-> Guest POD Wire Types & State Enum
// ============================================================================

#[derive(States, Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
enum GamePhase {
    #[default]
    Menu,
    Playing,
    Paused,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bridges_e2e::BridgeTelemetry")]
struct BridgeTelemetry {
    playing_ticks: u32,
    enter_menu: u32,
    enter_playing: u32,
    enter_paused: u32,
    exit_menu: u32,
    exit_playing: u32,
    trans_playing_paused: u32,
    last_state_disc: u32,
    last_virtual_dt_secs: f32,
    _pad0: u32,
    last_virtual_elapsed_secs: f64,
    saw_virtual_paused: u32,
    last_fixed_timestep_secs: f32,
    last_real_dt_secs: f32,
    saw_key_w_pressed: u32,
    saw_key_w_just_pressed: u32,
    saw_mouse_left_just_pressed: u32,
    child_gt_x: f32,
    child_gt_y: f32,
    child_gt_z: f32,
    saw_child_name_len: u32,
    saw_child_vis_hidden: u32,
    asset_loaded_alpha_ok: u32,
    asset_cross_mod_rejected: u32,
    asset_traversal_rejected: u32,
    on_enter_playing_spawned: u32,
    init_error_code: i32,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bridges_e2e::DriveStateCmd")]
struct DriveStateCmd {
    target_discriminant: u32,
    apply: u32,
}

#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bridges_e2e::BetaTelemetry")]
struct BetaTelemetry {
    asset_loaded_beta_ok: u32,
    asset_cross_mod_rejected: u32,
    asset_traversal_rejected: u32,
    init_error_code: i32,
    /// Set to `1` once the guest's `Update`-tick `h.is_loaded(ctx.host())` observes `LOADED`.
    asset_update_tick_loaded: u32,
    /// Most recent non-OK `FfiResult` code from `Update`-tick `ctx.host().asset_get_load_state`.
    asset_update_tick_error: u32,
}

#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bridges_e2e::HostScopedTag")]
struct HostScopedTag {
    kind: u32,
}

#[cfg(feature = "bridge-reflect")]
mod reflect_assets {
    use bevy_asset::io::Reader;
    use bevy_asset::{Asset, AssetLoader, LoadContext};
    use bevy_reflect::TypePath;

    #[derive(Asset, TypePath, Debug, Clone)]
    pub(crate) struct TextAsset(pub(crate) String);

    #[derive(Default, TypePath)]
    pub(crate) struct TextAssetLoader;

    impl AssetLoader for TextAssetLoader {
        type Asset = TextAsset;
        type Settings = ();
        type Error = std::io::Error;

        async fn load(
            &self,
            reader: &mut dyn Reader,
            _settings: &(),
            _load_context: &mut LoadContext<'_>,
        ) -> Result<Self::Asset, Self::Error> {
            let mut bytes = Vec::new();
            reader.read_to_end(&mut bytes).await?;
            let text = String::from_utf8(bytes)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
            Ok(TextAsset(text))
        }

        fn extensions(&self) -> &[&str] {
            &["txt"]
        }
    }
}

// ============================================================================
// Temp Guest .cdylib Compiler (Cached Once Per Test Binary Process)
// ============================================================================

struct BuiltBridgeMods {
    _root: PathBuf,
    alpha_dylib: PathBuf,
    beta_dylib: PathBuf,
}

static BUILT_BRIDGE_MODS: OnceLock<BuiltBridgeMods> = OnceLock::new();
static TEMP_DIR_SEQ: AtomicUsize = AtomicUsize::new(0);

fn build_temp_guest_cdylib(
    parent_dir: &Path,
    crate_name: &str,
    guest_crate_dir: &Path,
    lockfile_path: &Path,
    shared_target_dir: &Path,
    lib_rs: &str,
) -> PathBuf {
    let crate_dir = parent_dir.join(crate_name);
    let src_dir = crate_dir.join("src");
    fs::create_dir_all(&src_dir).expect("create temp guest crate src dir");

    let guest_path_escaped = guest_crate_dir.display().to_string().replace('\\', "/");
    let cargo_toml = format!(
        r#"[workspace]

[package]
name = "{crate_name}"
version = "0.1.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = {{ path = "{guest_path_escaped}", features = ["derive"] }}
"#
    );
    fs::write(crate_dir.join("Cargo.toml"), cargo_toml).expect("write temp guest Cargo.toml");
    fs::write(src_dir.join("lib.rs"), lib_rs).expect("write temp guest src/lib.rs");
    if lockfile_path.is_file() {
        let _ = fs::copy(lockfile_path, crate_dir.join("Cargo.lock"));
    }

    let status = Command::new(env!("CARGO"))
        .arg("build")
        .arg("--manifest-path")
        .arg(crate_dir.join("Cargo.toml"))
        .arg("--target-dir")
        .arg(shared_target_dir)
        .status()
        .expect("invoke cargo build for temp bridge guest mod");
    assert!(
        status.success(),
        "cargo build failed for temp bridge guest mod '{crate_name}'"
    );

    let dylib = shared_target_dir
        .join("debug")
        .join(dylib_filename(crate_name));
    assert!(
        dylib.is_file(),
        "expected compiled cdylib at {}",
        dylib.display()
    );
    dylib
}

fn ensure_bridge_mods_built() -> &'static BuiltBridgeMods {
    BUILT_BRIDGE_MODS.get_or_init(|| {
        let ws = workspace_root();
        let guest_crate_dir = ws.join("crates").join("bevy_abi_mod_guest");
        let lockfile = ws.join("Cargo.lock");
        let shared_target = ws.join("target").join("bridges_e2e_guest_target");
        let build_root = std::env::temp_dir().join(format!(
            "bam_bridges_e2e_mods_{}_{}",
            std::process::id(),
            TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&build_root);
        fs::create_dir_all(&build_root).expect("create temp bridge mods root");

        let alpha_dylib = build_temp_guest_cdylib(
            &build_root,
            "alpha_bridge_mod",
            &guest_crate_dir,
            &lockfile,
            &shared_target,
            ALPHA_BRIDGE_MOD_SRC,
        );
        let beta_dylib = build_temp_guest_cdylib(
            &build_root,
            "beta_bridge_mod",
            &guest_crate_dir,
            &lockfile,
            &shared_target,
            BETA_BRIDGE_MOD_SRC,
        );

        BuiltBridgeMods {
            _root: build_root,
            alpha_dylib,
            beta_dylib,
        }
    })
}

struct TempAssetDir {
    path: PathBuf,
}

impl TempAssetDir {
    fn new(tag: &str) -> Self {
        let seq = TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "bam_bridges_e2e_assets_{tag}_{}_{}",
            std::process::id(),
            seq
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("create temp mod asset dir");
        Self { path }
    }
}

impl Drop for TempAssetDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

const ALPHA_BRIDGE_MOD_SRC: &str = r#"#![forbid(unsafe_code)]

use bevy_abi_mod_guest::bridges::{
    FfiKeyCode, FfiMouseButton, FfiTransform, FfiVisibility, GuestAssetHandle, GuestBridgesHostExt,
    GuestCommandsStateExt, GuestStateHandles, GuestStateQueryBuilderExt,
    GuestStateSystemBuilderExt, GuestStdCommandsExt, GuestStdQueryBuilderExt, GuestStdTypes,
    on_enter_schedule, on_exit_schedule, on_transition_schedule,
};
use bevy_abi_mod_guest::prelude::*;
use bevy_abi_mod_guest::raw::resolve_component;
use core::ffi::CStr;
use std::sync::Mutex;

#[repr(C)]
#[derive(ModComponent, Clone, Copy, Default)]
#[mod_component(name = "bridges_e2e::BridgeTelemetry")]
struct BridgeTelemetry {
    playing_ticks: u32,
    enter_menu: u32,
    enter_playing: u32,
    enter_paused: u32,
    exit_menu: u32,
    exit_playing: u32,
    trans_playing_paused: u32,
    last_state_disc: u32,
    last_virtual_dt_secs: f32,
    _pad0: u32,
    last_virtual_elapsed_secs: f64,
    saw_virtual_paused: u32,
    last_fixed_timestep_secs: f32,
    last_real_dt_secs: f32,
    saw_key_w_pressed: u32,
    saw_key_w_just_pressed: u32,
    saw_mouse_left_just_pressed: u32,
    child_gt_x: f32,
    child_gt_y: f32,
    child_gt_z: f32,
    saw_child_name_len: u32,
    saw_child_vis_hidden: u32,
    asset_loaded_alpha_ok: u32,
    asset_cross_mod_rejected: u32,
    asset_traversal_rejected: u32,
    on_enter_playing_spawned: u32,
    init_error_code: i32,
}

impl GuestComponent for BridgeTelemetry {
    const NAME: &'static CStr = c"bridges_e2e::BridgeTelemetry";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(ModComponent, Clone, Copy, Default)]
#[mod_component(name = "bridges_e2e::DriveStateCmd")]
struct DriveStateCmd {
    target_discriminant: u32,
    apply: u32,
}

impl GuestComponent for DriveStateCmd {
    const NAME: &'static CStr = c"bridges_e2e::DriveStateCmd";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(ModComponent, Clone, Copy, Default)]
#[mod_component(name = "bridges_e2e::HostScopedTag")]
struct HostScopedTag {
    kind: u32,
}

impl GuestComponent for HostScopedTag {
    const NAME: &'static CStr = c"bridges_e2e::HostScopedTag";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(AbiPod, Clone, Copy, Default)]
struct TrackedParent;

impl GuestComponent for TrackedParent {
    const NAME: &'static CStr = c"alpha_bridge_mod::TrackedParent";
}

#[repr(C)]
#[derive(AbiPod, Clone, Copy, Default)]
struct TrackedChild;

impl GuestComponent for TrackedChild {
    const NAME: &'static CStr = c"alpha_bridge_mod::TrackedChild";
}

struct AlphaRuntime {
    std: GuestStdTypes,
    state: GuestStateHandles,
    tracked_parent: BamComponent,
    tracked_child: BamComponent,
    child_of: BamComponent,
    host_scoped_tag: BamComponent,
    alpha_asset: Option<GuestAssetHandle>,
    cross_rejected: u32,
    traversal_rejected: u32,
    hierarchy_initialized: bool,
}

static RUNTIME: Mutex<Option<AlphaRuntime>> = Mutex::new(None);

#[derive(Default)]
struct AlphaBridgeMod;

fn check_ffi(res: FfiResult) -> Result<(), FfiResult> {
    if res.is_ok() { Ok(()) } else { Err(res) }
}

fn build_alpha(app: &mut GuestApp<'_>) -> Result<(), FfiResult> {
    let tracked_parent = app.register_component::<TrackedParent>()?;
    let tracked_child = app.register_component::<TrackedChild>()?;
    let child_of = app.resolve_component::<FfiChildOf>()?;
    let host_scoped_tag = app.resolve_component::<HostScopedTag>()?;

    let std = app.std_bridges()?;
    let state = GuestStateHandles::resolve_primary(app.host(), 3)?;
    let assets = app.assets();

    let alpha_asset = match resolve_component(app.host(), Some(c"TextAsset"), None) {
        Ok(text_comp) => {
            let rel_handle = assets.load_handle(text_comp, "greeting.txt")?;
            let explicit_handle =
                assets.load_handle(text_comp, "mod://alpha_bridge_mod/greeting.txt")?;
            let _ = explicit_handle.release(app.host());
            Some(rel_handle)
        }
        Err(_) => None,
    };

    let cross_rejected = u32::from(
        assets.load_untyped("mod://beta_bridge_mod/secret.txt") == Err(FfiResult::PERMISSION_DENIED),
    );
    let traversal_rejected = u32::from(
        assets.load_untyped("../beta_bridge_mod/secret.txt") == Err(FfiResult::PERMISSION_DENIED),
    );

    let bridge = state.bridge;
    *RUNTIME.lock().expect("lock alpha runtime") = Some(AlphaRuntime {
        std,
        state,
        tracked_parent,
        tracked_child,
        child_of,
        host_scoped_tag,
        alpha_asset,
        cross_rejected,
        traversal_rejected,
        hierarchy_initialized: false,
    });

    let base = ScheduleId::custom(2000);

    // 1. Startup: spawn parent + child hierarchy and DespawnOnEnter(Paused) entity.
    check_ffi(
        app.system(c"alpha_startup")
            .schedule(ScheduleId::STARTUP)
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                let (
                    std_handles,
                    enter_paused_marker,
                    tracked_parent,
                    tracked_child,
                    child_of,
                    host_scoped_tag,
                    cross,
                    trav,
                ) = {
                    let guard = RUNTIME.lock().expect("lock alpha runtime");
                    let rt = guard.as_ref().ok_or(FfiResult::INVALID_ARGUMENT)?;
                    (
                        rt.std.handles,
                        rt.state.enter_marker(2).ok_or(FfiResult::NOT_FOUND)?,
                        rt.tracked_parent,
                        rt.tracked_child,
                        rt.child_of,
                        rt.host_scoped_tag,
                        rt.cross_rejected,
                        rt.traversal_rejected,
                    )
                };

                {
                    let cmds = ctx.commands();
                    let parent = cmds.spawn_empty()?;
                    check_ffi(cmds.insert_default_transform(parent, &std_handles))?;
                    check_ffi(cmds.insert_default_visibility(parent, &std_handles))?;
                    check_ffi(cmds.insert_default_name(parent, &std_handles))?;
                    check_ffi(cmds.insert(parent, tracked_parent, &TrackedParent))?;

                    let child = cmds.spawn_empty()?;
                    check_ffi(cmds.insert_default_transform(child, &std_handles))?;
                    check_ffi(cmds.insert_default_visibility(child, &std_handles))?;
                    check_ffi(cmds.insert_default_name(child, &std_handles))?;
                    check_ffi(cmds.insert(child, child_of, &FfiChildOf::new(parent)))?;
                    check_ffi(cmds.insert(child, tracked_child, &TrackedChild))?;

                    let enter_paused_scoped = cmds.spawn_empty()?;
                    check_ffi(cmds.insert(
                        enter_paused_scoped,
                        host_scoped_tag,
                        &HostScopedTag { kind: 2 },
                    ))?;
                    check_ffi(cmds.despawn_on_enter(enter_paused_scoped, enter_paused_marker))?;
                }

                let tel = ctx
                    .resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?;
                tel.asset_cross_mod_rejected = cross;
                tel.asset_traversal_rejected = trav;
                Ok(())
            }),
    )?;

    // 2. Update: configure/read parent & child Transform, GlobalTransform, Visibility, Name.
    check_ffi(
        app.system(c"alpha_hierarchy_and_std_types")
            .schedule(ScheduleId::UPDATE)
            .query(|q| {
                q.write_transform()
                    .read_global_transform()
                    .write_visibility()
                    .write_name()
                    .with::<TrackedParent>()
                    .without::<TrackedChild>()
            })
            .query(|q| {
                q.write_transform()
                    .read_global_transform()
                    .write_visibility()
                    .write_name()
                    .with::<TrackedChild>()
            })
            .query(|q| {
                q.read_time_virtual()
                    .read_time_fixed()
                    .read_time_real()
                    .read_keyboard_input()
                    .read_mouse_input()
                    .read_state(&bridge)
                    .write_next_state(&bridge)
            })
            .resource_read::<DriveStateCmd>()
            .resource_write::<BridgeTelemetry>()
            .build(|mut ctx| -> Result<(), FfiResult> {
                let mut guard = RUNTIME.lock().expect("lock alpha runtime");
                let rt = guard.as_mut().ok_or(FfiResult::INVALID_ARGUMENT)?;
                let std = rt.std;

                if !rt.hierarchy_initialized {
                    let q_parent = ctx.query_mut(0).ok_or(FfiResult::OUT_OF_RANGE)?;
                    if let Some(mut chunk) = q_parent.next_chunk_view()
                        && !chunk.is_empty()
                    {
                        check_ffi(std.transform.write_chunk_row(
                            &mut chunk,
                            0,
                            0,
                            &FfiTransform::from_xyz(10.0, 2.0, -1.0),
                        ))?;
                        check_ffi(std.visibility.write_chunk_row(
                            &mut chunk,
                            2,
                            0,
                            &FfiVisibility::VISIBLE,
                        ))?;
                        check_ffi(std.name.write_chunk_row(&mut chunk, 3, 0, "AlphaParent"))?;
                    }

                    let q_child = ctx.query_mut(1).ok_or(FfiResult::OUT_OF_RANGE)?;
                    if let Some(mut chunk) = q_child.next_chunk_view()
                        && !chunk.is_empty()
                    {
                        check_ffi(std.transform.write_chunk_row(
                            &mut chunk,
                            0,
                            0,
                            &FfiTransform::from_xyz(1.5, 3.0, 4.0),
                        ))?;
                        check_ffi(std.visibility.write_chunk_row(
                            &mut chunk,
                            2,
                            0,
                            &FfiVisibility::HIDDEN,
                        ))?;
                        check_ffi(std.name.write_chunk_row(&mut chunk, 3, 0, "AlphaChild"))?;
                        rt.hierarchy_initialized = true;
                    }
                }

                let mut child_gt = [0.0f32; 3];
                let mut child_name_len = 0u32;
                let mut child_vis_hidden = 0u32;
                {
                    let q_child = ctx.query_mut(1).ok_or(FfiResult::OUT_OF_RANGE)?;
                    if let Some(mut chunk) = q_child.next_chunk_view()
                        && !chunk.is_empty()
                    {
                        if let Some(gt) = std.global_transform.read_chunk_row(&mut chunk, 1, 0)? {
                            child_gt = gt.translation();
                        }
                        if let Some(vis) = std.visibility.read_chunk_row(&mut chunk, 2, 0)?
                            && vis.is_hidden()
                        {
                            child_vis_hidden = 1;
                        }
                        if let Some(name) = std.name.read_chunk_row(&mut chunk, 3, 0)? {
                            child_name_len = name.len() as u32;
                        }
                    }
                }

                let t_virtual = std.time_virtual.read_query_resource(&mut ctx, 2, 0)?;
                let t_fixed = std.time_fixed.read_query_resource(&mut ctx, 2, 1)?;
                let t_real = std.time_real.read_query_resource(&mut ctx, 2, 2)?;
                let kb = std.keyboard.read_query_resource(&mut ctx, 2, 3)?;
                let mouse = std.mouse.read_query_resource(&mut ctx, 2, 4)?;
                let current_state = rt.state.bridge.current_in_query(&mut ctx, 2, 5)?;

                let cmd = *ctx
                    .resource::<DriveStateCmd>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?;
                if cmd.apply != 0 {
                    check_ffi(rt.state.bridge.set_state_if_different_in_query(
                        &mut ctx,
                        2,
                        6,
                        cmd.target_discriminant,
                    ))?;
                }

                let tel = ctx
                    .resource_mut::<BridgeTelemetry>(1)
                    .ok_or(FfiResult::OUT_OF_RANGE)?;
                tel.last_state_disc = current_state;
                tel.last_virtual_dt_secs = t_virtual.delta_secs;
                tel.last_virtual_elapsed_secs = t_virtual.elapsed_secs_f64;
                tel.saw_virtual_paused = u32::from(t_virtual.paused != FfiBool::FALSE);
                tel.last_fixed_timestep_secs = t_fixed.timestep_secs as f32;
                tel.last_real_dt_secs = t_real.delta_secs;
                tel.saw_key_w_pressed = u32::from(kb.pressed(FfiKeyCode::KEY_W));
                tel.saw_key_w_just_pressed = u32::from(kb.just_pressed(FfiKeyCode::KEY_W));
                tel.saw_mouse_left_just_pressed =
                    u32::from(mouse.just_pressed(FfiMouseButton::LEFT));
                tel.child_gt_x = child_gt[0];
                tel.child_gt_y = child_gt[1];
                tel.child_gt_z = child_gt[2];
                tel.saw_child_name_len = child_name_len;
                tel.saw_child_vis_hidden = child_vis_hidden;
                if rt.alpha_asset.is_some_and(|h| h.is_some()) {
                    tel.asset_loaded_alpha_ok = 1;
                }
                Ok(())
            }),
    )?;

    // 3. State lifecycle schedules: OnEnter, OnExit, OnTransition, in_state.
    check_ffi(
        app.system(c"alpha_on_enter_menu")
            .schedule(on_enter_schedule(base, 0))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .enter_menu += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_on_exit_menu")
            .schedule(on_exit_schedule(base, 3, 0))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .exit_menu += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_on_enter_playing")
            .schedule(on_enter_schedule(base, 1))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                let (exit_playing_marker, host_scoped_tag) = {
                    let guard = RUNTIME.lock().expect("lock alpha runtime");
                    let rt = guard.as_ref().ok_or(FfiResult::INVALID_ARGUMENT)?;
                    (
                        rt.state.exit_marker(1).ok_or(FfiResult::NOT_FOUND)?,
                        rt.host_scoped_tag,
                    )
                };
                {
                    let cmds = ctx.commands();
                    let scoped = cmds.spawn_empty()?;
                    check_ffi(cmds.insert(scoped, host_scoped_tag, &HostScopedTag { kind: 1 }))?;
                    check_ffi(cmds.despawn_on_exit(scoped, exit_playing_marker))?;
                }
                let tel = ctx
                    .resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?;
                tel.enter_playing += 1;
                tel.on_enter_playing_spawned += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_on_exit_playing")
            .schedule(on_exit_schedule(base, 3, 1))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .exit_playing += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_on_enter_paused")
            .schedule(on_enter_schedule(base, 2))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .enter_paused += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_on_transition_playing_to_paused")
            .schedule(on_transition_schedule(base, 3, 1, 2))
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .trans_playing_paused += 1;
                Ok(())
            }),
    )?;

    check_ffi(
        app.system(c"alpha_while_playing")
            .schedule(ScheduleId::UPDATE)
            .run_if_in_state(c"host::state.GamePhase.Playing")
            .resource_write::<BridgeTelemetry>()
            .build(|ctx| -> Result<(), FfiResult> {
                ctx.resource_mut::<BridgeTelemetry>(0)
                    .ok_or(FfiResult::OUT_OF_RANGE)?
                    .playing_ticks += 1;
                Ok(())
            }),
    )?;

    Ok(())
}

impl GuestPlugin for AlphaBridgeMod {
    const NAME: &'static CStr = c"alpha_bridge_mod";
    const VERSION: &'static CStr = c"0.1.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        match build_alpha(app) {
            Ok(()) => FfiResult::OK,
            Err(err) => err,
        }
    }
}

export_mod!(plugin: AlphaBridgeMod);
"#;

const BETA_BRIDGE_MOD_SRC: &str = r#"#![forbid(unsafe_code)]

use bevy_abi_mod_guest::bridges::GuestAssetHandle;
use bevy_abi_mod_guest::prelude::*;
use bevy_abi_mod_guest::raw::resolve_component;
use core::ffi::CStr;
use std::sync::Mutex;

#[repr(C)]
#[derive(ModComponent, Clone, Copy, Default)]
#[mod_component(name = "bridges_e2e::BetaTelemetry")]
struct BetaTelemetry {
    asset_loaded_beta_ok: u32,
    asset_cross_mod_rejected: u32,
    asset_traversal_rejected: u32,
    init_error_code: i32,
    asset_update_tick_loaded: u32,
    asset_update_tick_error: u32,
}

impl GuestComponent for BetaTelemetry {
    const NAME: &'static CStr = c"bridges_e2e::BetaTelemetry";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

static BETA_ASSET: Mutex<Option<GuestAssetHandle>> = Mutex::new(None);

#[derive(Default)]
struct BetaBridgeMod;

fn build_beta(app: &mut GuestApp<'_>) -> Result<(), FfiResult> {
    let assets = app.assets();
    let beta_handle = match resolve_component(app.host(), Some(c"TextAsset"), None) {
        Ok(text_comp) => Some(assets.load_handle(text_comp, "secret.txt")?),
        Err(_) => None,
    };
    let cross_rejected = u32::from(
        assets.load_untyped("mod://alpha_bridge_mod/greeting.txt")
            == Err(FfiResult::PERMISSION_DENIED),
    );
    let traversal_rejected = u32::from(
        assets.load_untyped("../alpha_bridge_mod/greeting.txt")
            == Err(FfiResult::PERMISSION_DENIED),
    );

    *BETA_ASSET.lock().expect("lock beta asset") = beta_handle;

    let res = app
        .system(c"beta_asset_poll")
        .schedule(ScheduleId::UPDATE)
        .resource_write::<BetaTelemetry>()
        .build(move |ctx| -> Result<(), FfiResult> {
            let handle = *BETA_ASSET.lock().expect("lock beta asset");
            let has_handle = handle.is_some_and(|h| h.is_some());
            // `Update`-tick load-state polling through the live per-invocation `ctx.host()`
            // (refreshed via `sys_host_ctx`, never the poisoned init-time `LoadContext` host ctx).
            let tick_state = handle
                .filter(GuestAssetHandle::is_some)
                .map(|h| h.is_loaded(ctx.host()));
            let tel = ctx
                .resource_mut::<BetaTelemetry>(0)
                .ok_or(FfiResult::OUT_OF_RANGE)?;
            tel.asset_cross_mod_rejected = cross_rejected;
            tel.asset_traversal_rejected = traversal_rejected;
            if has_handle {
                tel.asset_loaded_beta_ok = 1;
            }
            match tick_state {
                Some(Ok(true)) => tel.asset_update_tick_loaded = 1,
                Some(Err(err)) => tel.asset_update_tick_error = err.0,
                _ => {}
            }
            Ok(())
        });
    if res.is_ok() { Ok(()) } else { Err(res) }
}

impl GuestPlugin for BetaBridgeMod {
    const NAME: &'static CStr = c"beta_bridge_mod";
    const VERSION: &'static CStr = c"0.1.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        match build_beta(app) {
            Ok(()) => FfiResult::OK,
            Err(err) => err,
        }
    }
}

export_mod!(plugin: BetaBridgeMod);
"#;

// ============================================================================
// End-to-End Cross-Bridge Integration Tests
// ============================================================================

#[test]
fn cross_bridge_cdylib_mods_drive_std_types_isolated_assets_and_state_machine() {
    let built = ensure_bridge_mods_built();
    let alpha_assets = TempAssetDir::new("alpha");
    let beta_assets = TempAssetDir::new("beta");
    fs::write(alpha_assets.path.join("greeting.txt"), b"hello alpha mod!").unwrap();
    fs::write(beta_assets.path.join("secret.txt"), b"beta secret asset!").unwrap();

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        ModBridgesPlugin,
        AssetPlugin::default(),
        TransformPlugin,
        AbiModPlugin,
    ));

    #[cfg(feature = "bridge-reflect")]
    {
        app.init_asset::<reflect_assets::TextAsset>();
        app.init_asset_loader::<reflect_assets::TextAssetLoader>();
        app.register_mod_asset::<reflect_assets::TextAsset>()
            .expect("register TextAsset");
    }

    app.mount_mod_asset_dir("alpha_bridge_mod", &alpha_assets.path)
        .expect("mount alpha_bridge_mod asset dir");
    app.mount_mod_asset_dir("beta_bridge_mod", &beta_assets.path)
        .expect("mount beta_bridge_mod asset dir");

    app.register_mod_resource::<BridgeTelemetry>()
        .expect("register BridgeTelemetry");
    app.register_mod_resource::<DriveStateCmd>()
        .expect("register DriveStateCmd");
    app.register_mod_resource::<BetaTelemetry>()
        .expect("register BetaTelemetry");
    app.register_mod_component::<HostScopedTag>(ComponentOptions::new().with_default())
        .expect("register HostScopedTag");

    app.insert_resource(BridgeTelemetry::default());
    app.insert_resource(DriveStateCmd::default());
    app.insert_resource(BetaTelemetry::default());

    let mut time_virtual = Time::<Virtual>::default();
    time_virtual.advance_by(Duration::from_millis(16));
    app.insert_resource(time_virtual);

    let mut time_fixed = Time::<Fixed>::from_seconds(1.0 / 50.0);
    time_fixed.advance_by(Duration::from_millis(20));
    app.insert_resource(time_fixed);

    let mut time_real = Time::<Real>::default();
    time_real.advance_by(Duration::from_millis(33));
    app.insert_resource(time_real);

    let mut kb = ButtonInput::<KeyCode>::default();
    kb.press(KeyCode::KeyW);
    app.insert_resource(kb);

    let mut mouse = ButtonInput::<MouseButton>::default();
    mouse.press(MouseButton::Left);
    app.insert_resource(mouse);

    let _spec = app
        .expose_mod_state::<GamePhase>(
            ScheduleId::custom(2000),
            &[
                (0, "Menu", GamePhase::Menu),
                (1, "Playing", GamePhase::Playing),
                (2, "Paused", GamePhase::Paused),
            ],
        )
        .expect("expose GamePhase");

    // Also spawn host Disabled entities with DespawnOnExit(Playing) and DespawnOnEnter(Paused)
    // to confirm Disabled entities are still despawned on state transitions.
    let disabled_exit_playing = app
        .world_mut()
        .spawn((
            Disabled,
            HostScopedTag { kind: 99 },
            DespawnOnExit(GamePhase::Playing),
        ))
        .id();
    let disabled_enter_paused = app
        .world_mut()
        .spawn((
            Disabled,
            HostScopedTag { kind: 98 },
            DespawnOnEnter(GamePhase::Paused),
        ))
        .id();

    app.finish();
    app.cleanup();

    let alpha_id = app
        .load_mod_with(
            &built.alpha_dylib,
            LoadOptions::new().with_shadow_copy(true),
        )
        .expect("load alpha_bridge_mod");
    let beta_id = app
        .load_mod_with(&built.beta_dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load beta_bridge_mod");

    // Frame 1: Startup + OnEnter(Menu) + initial Update (writes Transform/Visibility/Name).
    // PostUpdate runs TransformPropagate after Update!
    app.update();

    {
        let tel = app.world().resource::<BridgeTelemetry>();
        assert_eq!(tel.init_error_code, 0);
        assert_eq!(tel.enter_menu, 1);
        assert_eq!(tel.enter_playing, 0);
        assert_eq!(tel.playing_ticks, 0);
        assert_eq!(tel.last_state_disc, 0);
        assert_eq!(tel.asset_cross_mod_rejected, 1);
        assert_eq!(tel.asset_traversal_rejected, 1);
        assert!((tel.last_virtual_dt_secs - 0.016).abs() < 1e-4);
        assert!((tel.last_fixed_timestep_secs - 0.02).abs() < 1e-4);
        assert!((tel.last_real_dt_secs - 0.033).abs() < 1e-4);
        assert_eq!(tel.saw_key_w_pressed, 1);
        assert_eq!(tel.saw_key_w_just_pressed, 1);
        assert_eq!(tel.saw_mouse_left_just_pressed, 1);
    }

    // Frame 2: Read back propagated GlobalTransform ([10 + 1.5, 2 + 3, -1 + 4] = [11.5, 5.0, 3.0])
    // and trigger Menu -> Playing transition via ModNextState!
    app.world_mut()
        .resource_mut::<DriveStateCmd>()
        .target_discriminant = 1;
    app.world_mut().resource_mut::<DriveStateCmd>().apply = 1;
    app.world_mut().resource_mut::<Time<Virtual>>().pause();
    app.update();

    {
        let tel = app.world().resource::<BridgeTelemetry>();
        assert!(
            (tel.child_gt_x - 11.5).abs() < 1e-4,
            "expected child_gt_x=11.5, got ({}, {}, {}), name_len={}, vis_hidden={}",
            tel.child_gt_x,
            tel.child_gt_y,
            tel.child_gt_z,
            tel.saw_child_name_len,
            tel.saw_child_vis_hidden
        );
        assert!((tel.child_gt_y - 5.0).abs() < 1e-4);
        assert!((tel.child_gt_z - 3.0).abs() < 1e-4);
        assert_eq!(tel.saw_child_name_len, "AlphaChild".len() as u32);
        assert_eq!(tel.saw_child_vis_hidden, 1);
        assert_eq!(tel.saw_virtual_paused, 1);
    }

    // Verify host ECS components directly match guest mutations.
    {
        let world = app.world_mut();
        let mut q = world.query::<(&Name, &Visibility, &Transform, &GlobalTransform)>();
        let items: Vec<_> = q.iter(world).collect();
        let child = items
            .iter()
            .find(|(n, _, _, _)| n.as_str() == "AlphaChild")
            .expect("AlphaChild entity");
        assert_eq!(*child.1, Visibility::Hidden);
        assert!((child.2.translation.x - 1.5).abs() < 1e-4);
        assert!((child.3.translation().x - 11.5).abs() < 1e-4);
        assert!((child.3.translation().y - 5.0).abs() < 1e-4);
        assert!((child.3.translation().z - 3.0).abs() < 1e-4);
    }

    // Frame 3: StateTransition applies Menu -> Playing!
    // Keep DriveStateCmd pointing to Playing (1) to verify NextState::set_if_different idempotency.
    app.update();
    assert_eq!(
        *app.world().resource::<State<GamePhase>>().get(),
        GamePhase::Playing
    );
    {
        let tel = app.world().resource::<BridgeTelemetry>();
        assert_eq!(tel.exit_menu, 1);
        assert_eq!(tel.enter_playing, 1);
        assert_eq!(tel.on_enter_playing_spawned, 1);
        assert_eq!(tel.playing_ticks, 1);
        assert_eq!(tel.last_state_disc, 1);
    }

    // Frame 4: Re-writing discriminant 1 via set_state_if_different must NOT re-trigger OnEnter(Playing).
    app.update();
    {
        let tel = app.world().resource::<BridgeTelemetry>();
        assert_eq!(tel.enter_playing, 1);
        assert_eq!(tel.exit_playing, 0);
        assert_eq!(tel.playing_ticks, 2);
    }

    // Confirm both kind=1 (DespawnOnExit(Playing)) and kind=2 (DespawnOnEnter(Paused)) and
    // Disabled kind=99 exist right now before transitioning to Paused.
    {
        let world = app.world_mut();
        let mut all_tags = world.query::<(&HostScopedTag, Has<Disabled>)>();
        let tags: Vec<_> = all_tags.iter(world).collect();
        assert!(tags.iter().any(|(t, d)| t.kind == 1 && !*d));
        assert!(tags.iter().any(|(t, d)| t.kind == 2 && !*d));
        assert!(tags.iter().any(|(t, d)| t.kind == 99 && *d));
        assert!(tags.iter().any(|(t, d)| t.kind == 98 && *d));
    }

    // Frame 5 & 6: Transition Playing -> Paused!
    app.world_mut()
        .resource_mut::<DriveStateCmd>()
        .target_discriminant = 2;
    app.world_mut().resource_mut::<DriveStateCmd>().apply = 1;
    app.update();
    app.update();

    assert_eq!(
        *app.world().resource::<State<GamePhase>>().get(),
        GamePhase::Paused
    );
    {
        let tel = app.world().resource::<BridgeTelemetry>();
        assert_eq!(tel.exit_playing, 1);
        assert_eq!(tel.enter_paused, 1);
        assert_eq!(tel.trans_playing_paused, 1);
        // playing_ticks ran on Frame 5 (while still Playing before PostUpdate synced), not Frame 6.
        assert_eq!(tel.playing_ticks, 3);
    }
    // Verify DespawnOnExit(Playing) (including Disabled) and DespawnOnEnter(Paused) despawned!
    {
        assert!(app.world().get_entity(disabled_exit_playing).is_err());
        assert!(app.world().get_entity(disabled_enter_paused).is_err());
        let world = app.world_mut();
        let mut all_tags = world.query::<&HostScopedTag>();
        assert_eq!(all_tags.iter(world).count(), 0);
    }

    // Frame 7 & 8: Transition Paused -> Menu.
    app.world_mut()
        .resource_mut::<DriveStateCmd>()
        .target_discriminant = 0;
    app.world_mut().resource_mut::<DriveStateCmd>().apply = 1;
    app.update();
    app.update();
    assert_eq!(
        *app.world().resource::<State<GamePhase>>().get(),
        GamePhase::Menu
    );
    assert_eq!(app.world().resource::<BridgeTelemetry>().enter_menu, 2);

    // Poll host Assets<TextAsset> until async mod:// asset loads complete, and verify the guest's
    // `Update`-tick `h.is_loaded(ctx.host())` (live `sys_host_ctx`) observes `LOADED` too, then
    // check multi-mod isolation + handle release on unload.
    #[cfg(feature = "bridge-reflect")]
    {
        let mut both_texts_loaded = false;
        let mut guest_tick_observed_loaded = false;
        for _ in 0..80 {
            app.update();
            let assets = app
                .world()
                .resource::<bevy_asset::Assets<reflect_assets::TextAsset>>();
            let texts: Vec<&str> = assets.iter().map(|(_, a)| a.0.as_str()).collect();
            both_texts_loaded |=
                texts.contains(&"hello alpha mod!") && texts.contains(&"beta secret asset!");
            guest_tick_observed_loaded = app
                .world()
                .resource::<BetaTelemetry>()
                .asset_update_tick_loaded
                == 1;
            if both_texts_loaded && guest_tick_observed_loaded {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            both_texts_loaded,
            "expected both isolated mod:// TextAssets to finish loading on AssetServer"
        );
        assert_eq!(
            app.world()
                .resource::<BetaTelemetry>()
                .asset_update_tick_error,
            0,
            "Update-tick ctx.host().asset_get_load_state must use the live sys_host_ctx"
        );
        assert!(
            guest_tick_observed_loaded,
            "guest Update-tick h.is_loaded(ctx.host()) must observe LOADED via sys_host_ctx"
        );
        assert_eq!(
            app.world()
                .resource::<BridgeTelemetry>()
                .asset_loaded_alpha_ok,
            1
        );
        assert_eq!(
            app.world().resource::<BetaTelemetry>().asset_loaded_beta_ok,
            1
        );
        {
            let asset_reg = app.world().resource::<ModAssetRegistry>();
            assert_eq!(
                asset_reg.retained_count_for_owner(Some("alpha_bridge_mod")),
                1
            );
            assert_eq!(
                asset_reg.retained_count_for_owner(Some("beta_bridge_mod")),
                1
            );
        }
    }

    {
        let beta_tel = app.world().resource::<BetaTelemetry>();
        assert_eq!(beta_tel.init_error_code, 0);
        assert_eq!(beta_tel.asset_cross_mod_rejected, 1);
        assert_eq!(beta_tel.asset_traversal_rejected, 1);
    }

    // Unload alpha_bridge_mod -> its retained handles drop to 0 while beta_bridge_mod keeps its own.
    app.unload_mod(alpha_id).expect("unload alpha_bridge_mod");
    #[cfg(feature = "bridge-reflect")]
    {
        let asset_reg = app.world().resource::<ModAssetRegistry>();
        assert_eq!(
            asset_reg.retained_count_for_owner(Some("alpha_bridge_mod")),
            0
        );
        assert_eq!(
            asset_reg.retained_count_for_owner(Some("beta_bridge_mod")),
            1
        );
    }

    app.unload_mod(beta_id).expect("unload beta_bridge_mod");
    #[cfg(feature = "bridge-reflect")]
    {
        let asset_reg = app.world().resource::<ModAssetRegistry>();
        assert_eq!(
            asset_reg.retained_count_for_owner(Some("beta_bridge_mod")),
            0
        );
    }
}

#[cfg(all(feature = "bridge-bsn", feature = "bridge-reflect"))]
#[test]
fn bridge_cdylib_bsn_scene_ready_observer_unload_cleanup_and_reflect_brp() {
    use bevy_ecs::entity::Entity;
    use bevy_ecs::hierarchy::Children;
    use bevy_mod_abi::bridges::bsn::{ModBsnAppExt, ModBsnPlugin};
    use bevy_mod_abi::bridges::reflect::{
        BRP_METHOD_GET_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, ModReflectPlugin,
        ModReflectWorldExt, handle_mod_brp_request,
    };
    use bevy_remote::BrpRequest;
    use bevy_scene::bsn;
    use serde_json::json;

    #[repr(C)]
    #[derive(Resource, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
    #[mod_component(name = "bridge_e2e::BridgeE2eTelemetry")]
    struct BridgeE2eTelemetry {
        spawned_root_bits: u64,
        prop_entity_bits: u64,
        ready_count: u32,
        init_ok: u32,
    }

    const BRIDGE_E2E_MOD_SRC: &str = r#"
#![forbid(unsafe_code)]

use std::ffi::CStr;
use std::sync::atomic::{AtomicU32, Ordering};
use bevy_abi_mod_guest::bridges::bsn::{GuestCommandsBsnExt, GuestScene};
use bevy_abi_mod_guest::prelude::*;

static READY_HITS: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
#[derive(ModComponent, Clone, Copy, Default)]
#[mod_component(name = "bridge_e2e::BridgeE2eTelemetry")]
pub struct BridgeE2eTelemetry {
    pub spawned_root_bits: u64,
    pub prop_entity_bits: u64,
    pub ready_count: u32,
    pub init_ok: u32,
}

impl GuestComponent for BridgeE2eTelemetry {
    const NAME: &'static CStr = c"bridge_e2e::BridgeE2eTelemetry";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[derive(Default, ModComponent)]
#[mod_component(name = "bridge_e2e_mod::PropStats")]
pub struct PropStats {
    pub hp: f32,
    pub label: String,
}

fn spawn_and_sync(ctx: &mut SystemContext<'_>) -> Result<(), FfiResult> {
    let host = *ctx.host();
    let mut spawned_root_bits = 0u64;
    let mut prop_entity_bits = 0u64;
    let already_spawned = {
        let tel = ctx
            .resource_mut::<BridgeE2eTelemetry>(0)
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        tel.spawned_root_bits != 0
    };
    if !already_spawned {
        let scene = GuestScene::resolve(&host, "bridge_e2e::prop_tower")?;
        let cmds = ctx.commands();
        let root = cmds.spawn_scene(&scene)?;
        let prop_e = cmds.spawn_empty()?;
        let rc = cmds.insert_dynamic_component(
            prop_e,
            PropStats {
                hp: 42.5,
                label: String::from("Beacon"),
            },
        );
        if rc.is_err() {
            return Err(rc);
        }
        spawned_root_bits = root.to_bits();
        prop_entity_bits = prop_e.to_bits();
    }
    let tel = ctx
        .resource_mut::<BridgeE2eTelemetry>(0)
        .ok_or(FfiResult::OUT_OF_RANGE)?;
    if spawned_root_bits != 0 {
        tel.spawned_root_bits = spawned_root_bits;
        tel.prop_entity_bits = prop_entity_bits;
    }
    tel.ready_count = READY_HITS.load(Ordering::Acquire);
    tel.init_ok = 1;
    Ok(())
}

#[derive(Default)]
pub struct BridgeE2eMod;

impl GuestPlugin for BridgeE2eMod {
    const NAME: &'static CStr = c"bridge_e2e_mod";
    const VERSION: &'static CStr = c"0.1.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        if let Err(err) = app.register_dynamic_component::<PropStats>() {
            return err;
        }
        if let Err(err) = app.observe_ready("bridge_ready", FfiEntity::NULL, |_target, _cmds| {
            READY_HITS.fetch_add(1, Ordering::AcqRel);
        }) {
            return err;
        }
        let sys_res = app
            .system(c"bridge_spawn_and_sync")
            .schedule(ScheduleId::UPDATE)
            .resource_write::<BridgeE2eTelemetry>()
            .build(spawn_and_sync);
        if sys_res.is_ok() { FfiResult::OK } else { sys_res }
    }
}

export_mod!(plugin: BridgeE2eMod);
"#;

    let ws = workspace_root();
    let guest_crate_dir = ws.join("crates").join("bevy_abi_mod_guest");
    let lockfile = ws.join("Cargo.lock");
    let shared_target = ws.join("target").join("bridges_e2e_guest_target");
    let build_root = std::env::temp_dir().join(format!(
        "bam_bridge_e2e_mod_{}_{}",
        std::process::id(),
        TEMP_DIR_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = fs::remove_dir_all(&build_root);
    fs::create_dir_all(&build_root).expect("create temp bridge mod root");

    let dylib = build_temp_guest_cdylib(
        &build_root,
        "bridge_e2e_mod",
        &guest_crate_dir,
        &lockfile,
        &shared_target,
        BRIDGE_E2E_MOD_SRC,
    );

    let mut app = App::new();
    app.add_plugins((
        TaskPoolPlugin::default(),
        AssetPlugin::default(),
        AbiModPlugin,
        ModBsnPlugin,
        ModReflectPlugin,
    ));
    app.register_mod_resource::<BridgeE2eTelemetry>()
        .expect("register BridgeE2eTelemetry");
    app.insert_resource(BridgeE2eTelemetry::default());
    app.expose_mod_scene_bsn(
        "bridge_e2e::prop_tower",
        bsn! {
            #TowerRoot
            Children [
                #TowerChild
            ]
        },
    )
    .expect("expose bridge_e2e::prop_tower");

    app.finish();
    app.cleanup();

    let mod_id = app
        .load_mod_with(&dylib, LoadOptions::new().with_shadow_copy(true))
        .expect("load bridge_e2e_mod");

    // Frame 1 spawns the BSN scene + PropStats entity (Ready observer fires on spawn flush).
    // Frame 2 syncs `READY_HITS` into `BridgeE2eTelemetry`.
    app.update();
    app.update();

    let tel = *app.world().resource::<BridgeE2eTelemetry>();
    assert_ne!(tel.spawned_root_bits, 0);
    assert_ne!(tel.prop_entity_bits, 0);
    assert!(
        tel.ready_count >= 2,
        "expected Ready for TowerRoot + TowerChild, got {}",
        tel.ready_count
    );

    let root_entity = Entity::from_bits(tel.spawned_root_bits);
    let prop_entity = Entity::from_bits(tel.prop_entity_bits);
    assert_eq!(
        app.world().get::<Name>(root_entity).map(Name::as_str),
        Some("TowerRoot")
    );
    assert_eq!(
        app.world().get::<ModScoped>(root_entity),
        Some(&ModScoped(mod_id))
    );
    let child_entity = app
        .world()
        .get::<Children>(root_entity)
        .expect("TowerRoot Children")[0];
    assert_eq!(
        app.world().get::<Name>(child_entity).map(Name::as_str),
        Some("TowerChild")
    );

    // Inspect and mutate the guest's `PropStats` witness component via BRP!
    let prop_h = app
        .world()
        .resource::<ModRegistry>()
        .get_by_name("bridge_e2e_mod::PropStats")
        .expect("PropStats registered by guest")
        .handle;
    app.world_mut()
        .register_witness_reflection(prop_h)
        .expect("register PropStats witness reflection");

    let inspect_resp = handle_mod_brp_request(
        app.world_mut(),
        &BrpRequest {
            method: BRP_METHOD_GET_COMPONENTS.to_owned(),
            id: Some(json!(1)),
            params: Some(json!({
                "entity": prop_entity.to_bits(),
                "components": ["bridge_e2e_mod::PropStats"]
            })),
        },
    )
    .expect("BRP inspect PropStats");
    assert_eq!(
        inspect_resp["components"]["bridge_e2e_mod::PropStats"]["label"],
        json!("Beacon")
    );

    let mutate_resp = handle_mod_brp_request(
        app.world_mut(),
        &BrpRequest {
            method: BRP_METHOD_MUTATE_COMPONENTS.to_owned(),
            id: Some(json!(2)),
            params: Some(json!({
                "entity": prop_entity.to_bits(),
                "components": {
                    "bridge_e2e_mod::PropStats": {
                        "hp": 99.0,
                        "label": "SuperBeacon"
                    }
                }
            })),
        },
    )
    .expect("BRP mutate PropStats");
    assert_eq!(mutate_resp, json!(null));

    let after_resp = handle_mod_brp_request(
        app.world_mut(),
        &BrpRequest {
            method: BRP_METHOD_GET_COMPONENTS.to_owned(),
            id: Some(json!(3)),
            params: Some(json!({
                "entity": prop_entity.to_bits(),
                "components": ["bridge_e2e_mod::PropStats"]
            })),
        },
    )
    .expect("BRP re-inspect PropStats");
    assert_eq!(
        after_resp["components"]["bridge_e2e_mod::PropStats"]["hp"],
        json!(99.0)
    );
    assert_eq!(
        after_resp["components"]["bridge_e2e_mod::PropStats"]["label"],
        json!("SuperBeacon")
    );

    // Unload the mod and verify both BSN root and child entities are cleaned up via `ModScoped`.
    app.unload_mod(mod_id).expect("unload bridge_e2e_mod");
    assert!(app.world().get_entity(root_entity).is_err());
    assert!(app.world().get_entity(child_entity).is_err());
    assert!(app.world().get_entity(prop_entity).is_err());

    let _ = fs::remove_dir_all(&build_root);
}
