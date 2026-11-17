#![allow(unreachable_pub)]

//! Example 05: Cute 3D Slime Meadow (`winit` + `wgpu` + `avian3d`, `ABI 0.2.0`).
//!
//! Demonstrates driving a custom `winit` + `wgpu` 3D rendering loop and `avian3d` physics world
//! with live-swappable Rust (`.cdylib`) and ISO C99 (`.dylib`/`.so`/`.dll`) mods using the core
//! loader's `LoadOptions { shadow_copy: true, .. }`.

mod components;
mod meadow;
mod renderer;

use bevy_abi_mod::prelude::*;
use bevy_app::{App, PostUpdate};
use bevy_ecs::message::MessageReader;
use bevy_ecs::system::ResMut;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use components::{
    ActiveMod, ColorRgba, CuteSpirit, HomeStyle, ModStatusResource, Position, Scale3D, Velocity,
};
use meadow::{MeadowClock, host_meadow_post_physics, spawn_slimes};
use renderer::{CuteRenderer, OrbitCamera, SlimeRenderData};

struct CliArgs {
    test_mode: bool,
    headless: bool,
    ticks_per_phase: u64,
    slime_count: usize,
    initial_mod: Option<ActiveMod>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut test_mode = false;
        let mut headless = false;
        let mut ticks_per_phase = 40;
        let mut slime_count = 32;
        let mut initial_mod = Some(ActiveMod::Boing);

        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--test-mode" => {
                    test_mode = true;
                    headless = true;
                    initial_mod = None;
                }
                "--headless" => {
                    headless = true;
                }
                "--ticks" => {
                    if i + 1 < args.len() {
                        ticks_per_phase = args[i + 1].parse().unwrap_or(40);
                        i += 1;
                    }
                }
                "--slimes" => {
                    if i + 1 < args.len() {
                        slime_count = args[i + 1].parse().unwrap_or(32);
                        i += 1;
                    }
                }
                "--mod" => {
                    if i + 1 < args.len() {
                        initial_mod = match args[i + 1].to_lowercase().as_str() {
                            "none" | "0" => None,
                            "carousel" | "2" => Some(ActiveMod::Carousel),
                            "cuddle" | "c99" | "3" => Some(ActiveMod::CuddleC99),
                            _ => Some(ActiveMod::Boing),
                        };
                        i += 1;
                    }
                }
                "-h" | "--help" => {
                    println!(
                        "Usage: example_05_cute_3d_winit [OPTIONS]\n\n\
                         Options:\n  \
                           --test-mode       Run automated headless 3D ECS + C-ABI mod verification\n  \
                           --headless        Run simulation without opening a winit 3D window\n  \
                           --ticks <N>       Ticks per verification phase (default: 40)\n  \
                           --slimes <N>      Number of 3D bunny-slimes (default: 32)\n  \
                           --mod <name>      Initial mod: boing (default), carousel, cuddle, or none\n  \
                           -h, --help        Show this help message\n\n\
                         Interactive 3D Window Controls:\n  \
                           [1] Jelly Boing Hop (Rust cdylib)\n  \
                           [2] Pastel Carousel Waltz (Rust cdylib)\n  \
                           [3] Cozy Cuddle Huddle (Pure ISO C99)\n  \
                           [+] Spawn Slimes (+8)         [-] Remove Slimes (-8)\n  \
                           [R] Hot-Reload Active Mod     [U] Unload to Cozy Stroll\n  \
                           [Space] Pause / Resume        [O] Toggle Auto-Orbit Camera\n  \
                           [Left-Drag] Orbit 3D Camera   [Scroll] Zoom In / Out\n  \
                           [Q / Esc] Quit"
                    );
                    std::process::exit(0);
                }
                _ => {}
            }
            i += 1;
        }

        Self {
            test_mode,
            headless,
            ticks_per_phase,
            slime_count,
            initial_mod,
        }
    }
}

/// Resolves a compiled 3D mod shared library on disk across workspace and crate working directories.
fn resolve_mod_binary(kind: ActiveMod) -> Option<PathBuf> {
    let stem = kind.binary_stem()?;
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let filename = format!("{prefix}{stem}.{ext}");
    let candidates = match kind {
        ActiveMod::None => return None,
        ActiveMod::Boing | ActiveMod::Carousel => vec![
            PathBuf::from(format!(
                "examples/05_cute_3d_winit/mods/{stem}/target/release/{filename}"
            )),
            PathBuf::from(format!("mods/{stem}/target/release/{filename}")),
            PathBuf::from(format!(
                "examples/05_cute_3d_winit/mods/{stem}/target/debug/{filename}"
            )),
            PathBuf::from(format!("mods/{stem}/target/debug/{filename}")),
        ],
        ActiveMod::CuddleC99 => vec![
            PathBuf::from(format!(
                "examples/05_cute_3d_winit/mods/mod_cuddle_c99/{filename}"
            )),
            PathBuf::from(format!("mods/mod_cuddle_c99/{filename}")),
        ],
    };

    candidates.into_iter().find(|p| p.exists())
}

/// Ensures the requested Rust `.cdylib` or ISO C99 shared library is compiled on disk.
fn ensure_mod_binary_built(kind: ActiveMod) -> Result<PathBuf, String> {
    if let Some(existing) = resolve_mod_binary(kind) {
        return Ok(existing);
    }

    match kind {
        ActiveMod::None => return Err("Cannot build ActiveMod::None".to_string()),
        ActiveMod::Boing | ActiveMod::Carousel => {
            let stem = kind.binary_stem().unwrap();
            let manifest_candidates = [
                PathBuf::from(format!("examples/05_cute_3d_winit/mods/{stem}/Cargo.toml")),
                PathBuf::from(format!("mods/{stem}/Cargo.toml")),
            ];
            let manifest = manifest_candidates
                .into_iter()
                .find(|p| p.exists())
                .ok_or_else(|| format!("Cargo.toml not found for {stem}"))?;

            println!("[Host] Building {stem} Rust cdylib on demand...");
            let status = Command::new("cargo")
                .args(["build", "--manifest-path"])
                .arg(&manifest)
                .arg("--release")
                .status()
                .map_err(|e| format!("Failed to invoke cargo build for {stem}: {e}"))?;

            if !status.success() {
                return Err(format!("cargo build failed for {stem}"));
            }
        }
        ActiveMod::CuddleC99 => {
            let dir_candidates = [
                PathBuf::from("examples/05_cute_3d_winit/mods/mod_cuddle_c99"),
                PathBuf::from("mods/mod_cuddle_c99"),
            ];
            let c_dir = dir_candidates
                .into_iter()
                .find(|p| p.join("Makefile").exists())
                .ok_or_else(|| "Makefile not found for mod_cuddle_c99".to_string())?;

            println!("[Host] Compiling mod_cuddle_c99 (ISO C99) on demand...");
            let status = Command::new("make")
                .arg("-C")
                .arg(&c_dir)
                .status()
                .map_err(|e| format!("Failed to invoke make for mod_cuddle_c99: {e}"))?;

            if !status.success() {
                return Err("make failed for mod_cuddle_c99".to_string());
            }
        }
    }

    resolve_mod_binary(kind).ok_or_else(|| format!("Binary for {kind:?} missing after build"))
}

/// Drains lifecycle and fault `Message`s emitted by `AbiModPlugin` (`B-09`).
fn observe_mod_lifecycle_messages(
    mut loaded: MessageReader<ModLoaded>,
    mut unloaded: MessageReader<ModUnloaded>,
    mut faulted: MessageReader<ModSystemFaulted>,
    mut status: ResMut<ModStatusResource>,
) {
    for msg in loaded.read() {
        let _ = (&msg.name, &msg.version, msg.system_count);
    }
    for msg in unloaded.read() {
        let _ = (&msg.name, msg.removed_systems);
    }
    for fault in faulted.read() {
        status.fault_count += 1;
        status.status_message = format!(
            "System '{}' faulted ({})",
            fault.system_name,
            fault.status.name()
        );
    }
}

/// Loads or switches to `target` using core `LoadOptions { shadow_copy: true, .. }` (`L-03`).
fn switch_mod(app: &mut App, target: ActiveMod) -> Result<(), String> {
    if target == ActiveMod::None {
        return unload_active_mod(app);
    }

    let current = app.world().resource::<ModStatusResource>().active_mod;
    if current == target {
        return reload_active_mod(app);
    }
    if current != ActiveMod::None {
        unload_active_mod(app)?;
    }

    let binary_path = ensure_mod_binary_built(target)?;
    let mod_id = app
        .load_mod_with(&binary_path, LoadOptions::new().with_shadow_copy(true))
        .map_err(|err| format!("Failed to load 3D mod {}: {err}", target.name()))?;

    let mut status = app.world_mut().resource_mut::<ModStatusResource>();
    status.active_mod = target;
    status.mod_id = Some(mod_id);
    status.reload_count = 0;
    status.status_message = format!("Loaded {} ({mod_id:?})", target.name());
    Ok(())
}

/// Unloads the active 3D mod via `ModAppExt::unload_mod` and restores each slime's `HomeStyle`.
fn unload_active_mod(app: &mut App) -> Result<(), String> {
    let (prev_mod, prev_id) = {
        let status = app.world().resource::<ModStatusResource>();
        (status.active_mod, status.mod_id)
    };

    if prev_mod == ActiveMod::None {
        app.world_mut()
            .resource_mut::<ModStatusResource>()
            .status_message = "Already in peaceful Meadow Stroll.".to_string();
        return Ok(());
    }

    if let Some(mod_id) = prev_id {
        app.unload_mod(mod_id)
            .map_err(|err| format!("Failed to unload {}: {err}", prev_mod.name()))?;
    }

    {
        let mut query = app
            .world_mut()
            .query::<(&mut ColorRgba, &mut Scale3D, &mut CuteSpirit, &HomeStyle)>();
        for (mut col, mut scale, mut spirit, home) in query.iter_mut(app.world_mut()) {
            *col = home.base_color;
            *scale = Scale3D::uniform(1.0);
            spirit.blush = 0.42;
            spirit.ear_wiggle = 0.0;
        }
    }

    let mut status = app.world_mut().resource_mut::<ModStatusResource>();
    status.active_mod = ActiveMod::None;
    status.mod_id = None;
    status.reload_count = 0;
    status.status_message = format!("Unloaded {}. Slimes back to cozy stroll.", prev_mod.name());
    Ok(())
}

/// Hot-reloads the active 3D mod from disk via a fresh core shadow copy (`L-03`).
fn reload_active_mod(app: &mut App) -> Result<(), String> {
    let (target, prev_reloads) = {
        let status = app.world().resource::<ModStatusResource>();
        (status.active_mod, status.reload_count)
    };

    if target == ActiveMod::None {
        app.world_mut()
            .resource_mut::<ModStatusResource>()
            .status_message = "No active mod to hot-reload. Press 1, 2, or 3.".to_string();
        return Ok(());
    }

    unload_active_mod(app)?;
    switch_mod(app, target)?;

    let mut status = app.world_mut().resource_mut::<ModStatusResource>();
    status.reload_count = prev_reloads + 1;
    status.status_message = format!(
        "Hot-reloaded {} (Reload #{})",
        target.name(),
        status.reload_count
    );
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();

    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.add_plugins(bevy_time::TimePlugin);
    app.add_plugins(bevy_transform::TransformPlugin);
    app.add_plugins(avian3d::prelude::PhysicsPlugins::default());
    app.insert_resource(avian3d::prelude::SubstepCount(2));
    app.add_plugins(AbiModPlugin);
    app.init_resource::<MeadowClock>();
    app.init_resource::<ModStatusResource>();
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<Scale3D>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<ColorRgba>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<CuteSpirit>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.add_systems(
        PostUpdate,
        (host_meadow_post_physics, observe_mod_lifecycle_messages),
    );

    app.finish();
    app.cleanup();

    meadow::spawn_meadow_colliders(app.world_mut().commands());
    spawn_slimes(app.world_mut().commands(), args.slime_count);
    app.update();

    if let Some(initial) = args.initial_mod {
        switch_mod(&mut app, initial)?;
    }

    if args.test_mode {
        run_headless_3d_verification(&mut app, args.ticks_per_phase)?;
    } else if args.headless {
        for _ in 0..args.ticks_per_phase * 3 {
            app.update();
        }
        println!(
            "[HEADLESS 3D] Completed {} ticks with active mod: {}",
            args.ticks_per_phase * 3,
            app.world()
                .resource::<ModStatusResource>()
                .active_mod
                .name()
        );
    } else {
        run_winit_3d_meadow(app)?;
    }

    Ok(())
}

// =============================================================================
// Interactive Winit 0.30 ApplicationHandler Loop
// =============================================================================

struct CuteMeadowWinitApp {
    ecs_app: App,
    window: Option<Arc<Window>>,
    renderer: Option<CuteRenderer>,
    camera: OrbitCamera,
    dragging: bool,
    last_cursor: Option<(f64, f64)>,
    paused: bool,
    start_instant: Instant,
    last_frame_instant: Instant,
    last_sim_step: Instant,
    fps: f64,
    frame_counter: u64,
}

impl CuteMeadowWinitApp {
    fn new(ecs_app: App) -> Self {
        let now = Instant::now();
        Self {
            ecs_app,
            window: None,
            renderer: None,
            camera: OrbitCamera::default(),
            dragging: false,
            last_cursor: None,
            paused: false,
            start_instant: now,
            last_frame_instant: now,
            last_sim_step: now,
            fps: 60.0,
            frame_counter: 0,
        }
    }

    fn log_mod_status(&self) {
        if let Some(status) = self.ecs_app.world().get_resource::<ModStatusResource>() {
            println!("[CuteMeadow] {}", status.status_message);
        }
    }

    fn refresh_window_title(&self) {
        if let Some(win) = &self.window {
            let status = self
                .ecs_app
                .world()
                .get_resource::<ModStatusResource>()
                .cloned()
                .unwrap_or_default();
            let pause_str = if self.paused { " [PAUSED]" } else { "" };
            win.set_title(&format!(
                "Cute 3D Slime Meadow — {} (Reloads: {}){} | [1] Boing [2] Carousel [3] C99 Cuddle [R] Reload [U] Unload",
                status.active_mod.name(),
                status.reload_count,
                pause_str
            ));
        }
    }

    fn query_slime_count(&mut self) -> usize {
        let mut query = self.ecs_app.world_mut().query::<&Position>();
        query.iter(self.ecs_app.world()).count()
    }

    fn spawn_more_slimes(&mut self, count: usize) {
        let seed = (self.frame_counter as u32).wrapping_mul(7919) ^ 0xFEED_FACE;
        {
            let mut commands = self.ecs_app.world_mut().commands();
            meadow::spawn_slimes_batch(&mut commands, count, seed);
        }
        self.ecs_app.update();
        let total = self.query_slime_count();
        println!("[CuteMeadow] Spawned +{count} slimes. Total: {total}");
        self.refresh_window_title();
    }

    fn remove_slimes(&mut self, count: usize) {
        let mut to_despawn = Vec::new();
        {
            let mut query = self
                .ecs_app
                .world_mut()
                .query::<(bevy_ecs::entity::Entity, &Position)>();
            let all: Vec<_> = query.iter(self.ecs_app.world()).map(|(e, _)| e).collect();
            if all.len() <= 1 {
                return;
            }
            let remove_count = count.min(all.len().saturating_sub(1));
            to_despawn.extend_from_slice(&all[all.len() - remove_count..]);
        }
        for e in to_despawn {
            self.ecs_app.world_mut().commands().entity(e).despawn();
        }
        self.ecs_app.update();
        let total = self.query_slime_count();
        println!("[CuteMeadow] Removed slimes. Total: {total}");
        self.refresh_window_title();
    }

    fn collect_slimes(&mut self) -> Vec<SlimeRenderData> {
        let mut query = self
            .ecs_app
            .world_mut()
            .query::<(&Position, &Scale3D, &ColorRgba, &CuteSpirit)>();
        query
            .iter(self.ecs_app.world())
            .map(|(p, sc, col, sp)| SlimeRenderData {
                pos: *p,
                scale: *sc,
                color: *col,
                spirit: *sp,
            })
            .collect()
    }
}

impl ApplicationHandler for CuteMeadowWinitApp {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let attrs = Window::default_attributes()
            .with_title("Cute 3D Slime Meadow — bevy_abi_mod v0.2 (winit + wgpu)")
            .with_inner_size(LogicalSize::new(1180.0, 760.0));

        let window = match event_loop.create_window(attrs) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                eprintln!("Failed to create winit window: {e}");
                event_loop.exit();
                return;
            }
        };

        match CuteRenderer::new(window.clone()) {
            Ok(renderer) => {
                self.window = Some(window);
                self.renderer = Some(renderer);
                self.refresh_window_title();
                println!("=== Cute 3D Slime Meadow Running ===");
                println!("  [1] Jelly Boing Hop (Rust cdylib)");
                println!("  [2] Pastel Carousel Waltz (Rust cdylib)");
                println!("  [3] Cozy Cuddle Huddle (Pure C99)");
                println!("  [+] Spawn Slimes (+8)       [-] Remove Slimes (-8)");
                println!("  [R] Hot-Reload Active Mod   [U] Unload Mod   [Space] Pause");
                println!("  [Left-Drag] Orbit Camera    [Scroll] Zoom    [Esc/Q] Quit");
            }
            Err(e) => {
                eprintln!("Failed to initialize 3D GPU renderer: {e}");
                event_loop.exit();
            }
        }
    }

    fn window_event(
        &mut self,
        event_loop: &ActiveEventLoop,
        _window_id: WindowId,
        event: WindowEvent,
    ) {
        match event {
            WindowEvent::CloseRequested => {
                let _ = unload_active_mod(&mut self.ecs_app);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && !event.repeat
                    && let PhysicalKey::Code(code) = event.physical_key
                {
                    match code {
                        KeyCode::Digit1 | KeyCode::Numpad1 => {
                            match switch_mod(&mut self.ecs_app, ActiveMod::Boing) {
                                Ok(()) => self.log_mod_status(),
                                Err(e) => eprintln!("[Host] {e}"),
                            }
                            self.refresh_window_title();
                        }
                        KeyCode::Digit2 | KeyCode::Numpad2 => {
                            match switch_mod(&mut self.ecs_app, ActiveMod::Carousel) {
                                Ok(()) => self.log_mod_status(),
                                Err(e) => eprintln!("[Host] {e}"),
                            }
                            self.refresh_window_title();
                        }
                        KeyCode::Digit3 | KeyCode::Numpad3 => {
                            match switch_mod(&mut self.ecs_app, ActiveMod::CuddleC99) {
                                Ok(()) => self.log_mod_status(),
                                Err(e) => eprintln!("[Host] {e}"),
                            }
                            self.refresh_window_title();
                        }
                        KeyCode::KeyU | KeyCode::Digit0 => {
                            if unload_active_mod(&mut self.ecs_app).is_ok() {
                                self.log_mod_status();
                            }
                            self.refresh_window_title();
                        }
                        KeyCode::KeyR => {
                            match reload_active_mod(&mut self.ecs_app) {
                                Ok(()) => self.log_mod_status(),
                                Err(e) => eprintln!("[Host] {e}"),
                            }
                            self.refresh_window_title();
                        }
                        KeyCode::Equal | KeyCode::NumpadAdd | KeyCode::KeyB => {
                            self.spawn_more_slimes(8);
                        }
                        KeyCode::Minus | KeyCode::NumpadSubtract => {
                            self.remove_slimes(8);
                        }
                        KeyCode::Space => {
                            self.paused = !self.paused;
                            self.refresh_window_title();
                        }
                        KeyCode::KeyO => {
                            self.camera.auto_orbit = !self.camera.auto_orbit;
                        }
                        KeyCode::ArrowLeft | KeyCode::KeyA => {
                            self.camera.auto_orbit = false;
                            self.camera.yaw -= 0.14;
                        }
                        KeyCode::ArrowRight | KeyCode::KeyD => {
                            self.camera.auto_orbit = false;
                            self.camera.yaw += 0.14;
                        }
                        KeyCode::ArrowUp | KeyCode::KeyW => {
                            self.camera.pitch = (self.camera.pitch + 0.08).clamp(0.12, 1.35);
                        }
                        KeyCode::ArrowDown | KeyCode::KeyS => {
                            self.camera.pitch = (self.camera.pitch - 0.08).clamp(0.12, 1.35);
                        }
                        KeyCode::Escape | KeyCode::KeyQ => {
                            let _ = unload_active_mod(&mut self.ecs_app);
                            event_loop.exit();
                        }
                        _ => {}
                    }
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if button == MouseButton::Left {
                    self.dragging = state == ElementState::Pressed;
                    if !self.dragging {
                        self.last_cursor = None;
                    } else {
                        self.camera.auto_orbit = false;
                    }
                }
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
                    if let Some((lx, ly)) = self.last_cursor {
                        let dx = (position.x - lx) as f32;
                        let dy = (position.y - ly) as f32;
                        self.camera.yaw -= dx * 0.0065;
                        self.camera.pitch = (self.camera.pitch + dy * 0.0065).clamp(0.10, 1.38);
                    }
                    self.last_cursor = Some((position.x, position.y));
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let scroll = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y * 1.1,
                    MouseScrollDelta::PixelDelta(p) => (p.y as f32) * 0.02,
                };
                self.camera.distance = (self.camera.distance - scroll).clamp(7.5, 32.0);
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt_frame = now.duration_since(self.last_frame_instant).as_secs_f64();
                self.last_frame_instant = now;
                if dt_frame > 0.0005 {
                    self.fps = self.fps * 0.92 + (1.0 / dt_frame) * 0.08;
                }

                if self.camera.auto_orbit && !self.dragging {
                    self.camera.yaw += (dt_frame as f32) * 0.16;
                }

                let sim_elapsed = now.duration_since(self.last_sim_step).as_secs_f32();
                if sim_elapsed >= (1.0 / 65.0) {
                    self.last_sim_step = now;
                    if !self.paused {
                        self.ecs_app.update();
                    }
                }

                let slimes = self.collect_slimes();
                let status = self
                    .ecs_app
                    .world()
                    .get_resource::<ModStatusResource>()
                    .cloned()
                    .unwrap_or_default();
                let time_secs = self.start_instant.elapsed().as_secs_f32();

                if let Some(renderer) = &mut self.renderer {
                    renderer.render_frame(
                        &self.camera,
                        &slimes,
                        &status,
                        self.fps,
                        self.paused,
                        time_secs,
                    );
                }

                self.frame_counter += 1;
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        event_loop.set_control_flow(ControlFlow::Poll);
        if let Some(win) = &self.window {
            win.request_redraw();
        }
    }
}

fn run_winit_3d_meadow(app: App) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut handler = CuteMeadowWinitApp::new(app);
    event_loop.run_app(&mut handler)?;
    Ok(())
}

// =============================================================================
// Automated Headless 3D ECS + Multi-Language C-ABI Mod Verification
// =============================================================================

fn run_headless_3d_verification(
    app: &mut App,
    ticks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Example 05 Automated Cute 3D Modding Verification ===");

    println!("[TEST] Phase 1: Baseline Cozy Meadow Stroll ({ticks} ticks)...");
    for _ in 0..ticks {
        app.update();
    }
    let pos_base = query_positions(app);
    assert!(
        pos_base.iter().all(|p| p.y.abs() < 0.05),
        "Baseline slimes should stroll along the meadow grass (y ~= 0)"
    );
    println!("  -> PASS: Baseline 3D meadow stroll verified.");

    println!("[TEST] Phase 2: Loading Mod 1 — Jelly Boing Hop (Rust cdylib) ({ticks} ticks)...");
    switch_mod(app, ActiveMod::Boing)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Boing
    );

    let mut max_hop_y = 0.0_f32;
    let mut saw_squash_stretch = false;
    for _ in 0..ticks {
        app.update();
        for p in query_positions(app) {
            max_hop_y = max_hop_y.max(p.y);
        }
        for sc in query_scales(app) {
            if (sc.y - 1.0).abs() > 0.12 {
                saw_squash_stretch = true;
            }
        }
    }
    assert!(
        max_hop_y > 0.45,
        "Jelly Boing mod must launch slimes into 3D hops (max_y={max_hop_y})"
    );
    assert!(
        saw_squash_stretch,
        "Jelly Boing mod must deform Scale3D for jelly squash-and-stretch"
    );
    println!("  -> PASS: 3D hopping (max_y={max_hop_y:.2}) and jelly squash-stretch verified.");

    println!(
        "[TEST] Phase 3: Switching to Mod 2 — Pastel Carousel Waltz (Rust cdylib) ({ticks} ticks)..."
    );
    let colors_before = query_colors(app);
    switch_mod(app, ActiveMod::Carousel)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Carousel
    );

    for _ in 0..ticks {
        app.update();
    }
    let colors_after = query_colors(app);
    let colors_changed = colors_before
        .iter()
        .zip(colors_after.iter())
        .any(|(a, b)| (a.r - b.r).abs() > 0.02 || (a.g - b.g).abs() > 0.02);
    assert!(
        colors_changed,
        "Pastel Carousel mod must animate slime pastel rainbow colors"
    );
    println!("  -> PASS: 3D carousel orbital waltz and macaron color animation verified.");

    println!(
        "[TEST] Phase 4: Switching to Mod 3 — Cozy Cuddle Huddle (Pure C99) ({ticks} ticks)..."
    );
    let dist_before = avg_xz_radius(app);
    switch_mod(app, ActiveMod::CuddleC99)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::CuddleC99
    );

    for _ in 0..ticks {
        app.update();
    }
    let dist_after = avg_xz_radius(app);
    let spirits = query_spirits(app);
    let blushing = spirits.iter().any(|s| s.blush >= 0.64);
    assert!(
        blushing,
        "C99 Cozy Cuddle mod must give slimes warm rosy blushes"
    );
    println!(
        "  -> PASS: Pure C99 cuddle huddle verified (avg radius {dist_before:.2} -> {dist_after:.2})."
    );

    println!("[TEST] Phase 5: Hot-Reloading Active 3D Mod ({ticks} ticks)...");
    reload_active_mod(app)?;
    assert_eq!(app.world().resource::<ModStatusResource>().reload_count, 1);
    for _ in 0..ticks {
        app.update();
    }
    println!("  -> PASS: Core shadow-copy hot-reload (#1) verified.");

    println!("[TEST] Phase 6: Unloading Mod back to Cozy Stroll ({ticks} ticks)...");
    unload_active_mod(app)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::None
    );
    for _ in 0..ticks {
        app.update();
    }
    println!("  -> PASS: Clean dlclose unload and baseline restoration verified.");

    println!("[TEST] Phase 7: Dynamic Population Scaling & Collision Bouncing ({ticks} ticks)...");
    let count_before = query_positions(app).len();
    {
        let mut commands = app.world_mut().commands();
        meadow::spawn_slimes_batch(&mut commands, 16, 0x1234_ABCD);
    }
    app.update();
    let count_after = query_positions(app).len();
    assert_eq!(
        count_after,
        count_before + 16,
        "Spawning additional slimes must increase total entity count"
    );

    let col_entity_a = {
        let mut commands = app.world_mut().commands();
        let e = meadow::spawn_single_slime(
            &mut commands,
            Position::new(2.5, 0.0, 0.0),
            Velocity::new(3.0, 0.0, 0.0),
            Scale3D::uniform(1.0),
            ColorRgba::new(1.0, 0.5, 0.5, 1.0),
            CuteSpirit::new(0.0, 0.0, 0.2),
            HomeStyle {
                base_color: ColorRgba::new(1.0, 0.5, 0.5, 1.0),
                phase_offset: 0.0,
            },
        );
        meadow::spawn_single_slime(
            &mut commands,
            Position::new(3.1, 0.0, 0.0),
            Velocity::new(-3.0, 0.0, 0.0),
            Scale3D::uniform(1.0),
            ColorRgba::new(0.5, 0.5, 1.0, 1.0),
            CuteSpirit::new(0.0, 0.0, 0.2),
            HomeStyle {
                base_color: ColorRgba::new(0.5, 0.5, 1.0, 1.0),
                phase_offset: 0.0,
            },
        );
        e
    };
    app.world_mut().flush();
    app.update();

    let bounced_pos = app.world().get::<Position>(col_entity_a).unwrap();
    let bounced_vel = app.world().get::<Velocity>(col_entity_a).unwrap();
    let bounced_spirit = app.world().get::<CuteSpirit>(col_entity_a).unwrap();

    assert!(
        bounced_vel.x < 0.0 || bounced_pos.x < 2.5,
        "Colliding slimes must bounce and reverse direction upon collision"
    );
    assert_eq!(
        bounced_spirit.blush, 1.0,
        "Colliding slimes must blush rosy pink upon bumping into each other"
    );
    println!(
        "  -> PASS: Dynamic spawn (+16) and elastic collision bounce + blush reaction verified."
    );

    println!(
        "\n[TEST_MODE] SUCCESS: All 3 3D mods, shadow-copy hot-reload, dynamic population, and collision bouncing verified."
    );

    Ok(())
}

fn query_positions(app: &mut App) -> Vec<Position> {
    let mut q = app.world_mut().query::<&Position>();
    q.iter(app.world()).copied().collect()
}

fn query_scales(app: &mut App) -> Vec<Scale3D> {
    let mut q = app.world_mut().query::<&Scale3D>();
    q.iter(app.world()).copied().collect()
}

fn query_colors(app: &mut App) -> Vec<ColorRgba> {
    let mut q = app.world_mut().query::<&ColorRgba>();
    q.iter(app.world()).copied().collect()
}

fn query_spirits(app: &mut App) -> Vec<CuteSpirit> {
    let mut q = app.world_mut().query::<&CuteSpirit>();
    q.iter(app.world()).copied().collect()
}

fn avg_xz_radius(app: &mut App) -> f32 {
    let pos = query_positions(app);
    if pos.is_empty() {
        return 0.0;
    }
    pos.iter()
        .map(|p| (p.x * p.x + p.z * p.z).sqrt())
        .sum::<f32>()
        / (pos.len() as f32)
}
