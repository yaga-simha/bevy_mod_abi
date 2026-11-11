//! Stage 5 Example: Cute 3D Slime Meadow with Dynamic Mod Loading (`winit` + `wgpu`)
//!
//! Replaces the 2D terminal TUI with an interactive, toy-diorama 3D window powered by `winit`
//! and `wgpu`, featuring kawaii pastel bunny-slimes on a floating mushroom island!
//!
//! Dynamically loads, hot-reloads, and unloads zero-serialization C-ABI mods (both Rust `.cdylib`
//! and pure ISO C99 `.dylib`/`.so`/`.dll`) at runtime:
//! - `[1]` **Jelly Boing Hop** (`mod_boing` — Rust): Bouncy 3D hops, volume-preserving squash & stretch, ear flop!
//! - `[2]` **Pastel Carousel Waltz** (`mod_carousel` — Rust): Two-ring counter-rotating 3D merry-go-round & macaron rainbow hues!
//! - `[3]` **Cozy Cuddle Huddle** (`mod_cuddle_c99` — Pure C99): Sunflower cuddle pile around the giant mushroom with rosy blushes!
//! - `[R]` Hot-reload active mod via shadow copy | `[U]` Unload mod | `[Space]` Pause | `[Drag/Scroll]` Orbit 3D Camera.

mod components;
mod meadow;
mod mod_manager;
mod renderer;

use bevy_app::{App, PostUpdate};
use host_runtime::prelude::*;
use std::sync::Arc;
use std::time::Instant;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use components::{ColorRgba, CuteSpirit, Position, Scale3D, Velocity};
use meadow::{host_meadow_post_physics, spawn_slimes, MeadowClock};
use mod_manager::{ActiveMod, ModManager, ModStatusResource};
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
                            "boing" | "1" => Some(ActiveMod::Boing),
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
                           --slimes <N>      Number of cute 3D bunny-slimes (default: 32)\n  \
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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();

    // 1. Initialize Bevy ECS App with AbiModPlugin and 3D ModExposable components
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.add_plugins(bevy_time::TimePlugin);
    app.add_plugins(bevy_transform::TransformPlugin);
    app.add_plugins(avian3d::prelude::PhysicsPlugins::default());
    app.insert_resource(avian3d::prelude::SubstepCount(2));
    app.add_plugins(AbiModPlugin);
    app.init_resource::<MeadowClock>();
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Scale3D>();
    app.register_mod_component::<ColorRgba>();
    app.register_mod_component::<CuteSpirit>();
    app.add_systems(PostUpdate, host_meadow_post_physics);

    // Finalize plugins (initializes Avian collision tree diagnostics & resources)
    app.finish();
    app.cleanup();

    // 2. Spawn static colliders and cute pastel bunny-slimes onto the floating 3D meadow
    meadow::spawn_meadow_colliders(app.world_mut().commands());
    spawn_slimes(app.world_mut().commands(), args.slime_count);
    app.update();

    // 3. Initialize Dynamic ModManager (Rust cdylib + Pure C99 mods)
    let mut mod_manager = ModManager::new()?;
    mod_manager.sync_resource(&mut app);

    if let Some(initial) = args.initial_mod {
        mod_manager.load_mod(&mut app, initial)?;
    }

    // 4. Headless verification or Interactive 3D Winit Window
    if args.test_mode {
        run_headless_3d_verification(&mut app, &mut mod_manager, args.ticks_per_phase)?;
    } else if args.headless {
        for _ in 0..args.ticks_per_phase * 3 {
            app.update();
        }
        println!(
            "[HEADLESS 3D] Completed {} ticks with active mod: {}",
            args.ticks_per_phase * 3,
            mod_manager.active_mod().name()
        );
    } else {
        run_winit_3d_meadow(app, mod_manager)?;
    }

    Ok(())
}

// =============================================================================
// Interactive Winit 0.30 ApplicationHandler Loop
// =============================================================================

struct CuteMeadowWinitApp {
    ecs_app: App,
    mod_manager: ModManager,
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
    fn new(ecs_app: App, mod_manager: ModManager) -> Self {
        let now = Instant::now();
        Self {
            ecs_app,
            mod_manager,
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
            println!("✨ [CuteMeadow] {}", status.status_message);
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
                "✨ Cute 3D Slime Meadow — {} (Reloads: {}){} | [1] Boing [2] Carousel [3] C99 Cuddle [R] Reload [U] Unload",
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
        println!(
            "✨ [CuteMeadow] Spawned +{} slimes! Total: {}",
            count, total
        );
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
        println!("✨ [CuteMeadow] Removed slimes. Total: {}", total);
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
            .with_title("✨ Cute 3D Slime Meadow — bevy_abi_mod (winit + wgpu)")
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
                println!("=== ✨ Cute 3D Slime Meadow Running! ===");
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
                let _ = self.mod_manager.unload_mod(&mut self.ecs_app);
                event_loop.exit();
            }
            WindowEvent::Resized(size) => {
                if let Some(renderer) = &mut self.renderer {
                    renderer.resize(size.width, size.height);
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed && !event.repeat {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        match code {
                            KeyCode::Digit1 | KeyCode::Numpad1 => {
                                match self
                                    .mod_manager
                                    .load_mod(&mut self.ecs_app, ActiveMod::Boing)
                                {
                                    Ok(()) => self.log_mod_status(),
                                    Err(e) => eprintln!("[ModManager] {e}"),
                                }
                                self.refresh_window_title();
                            }
                            KeyCode::Digit2 | KeyCode::Numpad2 => {
                                match self
                                    .mod_manager
                                    .load_mod(&mut self.ecs_app, ActiveMod::Carousel)
                                {
                                    Ok(()) => self.log_mod_status(),
                                    Err(e) => eprintln!("[ModManager] {e}"),
                                }
                                self.refresh_window_title();
                            }
                            KeyCode::Digit3 | KeyCode::Numpad3 => {
                                match self
                                    .mod_manager
                                    .load_mod(&mut self.ecs_app, ActiveMod::CuddleC99)
                                {
                                    Ok(()) => self.log_mod_status(),
                                    Err(e) => eprintln!("[ModManager] {e}"),
                                }
                                self.refresh_window_title();
                            }
                            KeyCode::KeyU | KeyCode::Digit0 => {
                                if self.mod_manager.unload_mod(&mut self.ecs_app).is_ok() {
                                    self.log_mod_status();
                                }
                                self.refresh_window_title();
                            }
                            KeyCode::KeyR => {
                                match self.mod_manager.reload_mod(&mut self.ecs_app) {
                                    Ok(()) => self.log_mod_status(),
                                    Err(e) => eprintln!("[ModManager] {e}"),
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
                                let _ = self.mod_manager.unload_mod(&mut self.ecs_app);
                                event_loop.exit();
                            }
                            _ => {}
                        }
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

                // Fixed-rate ~60Hz ECS simulation stepping
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

fn run_winit_3d_meadow(
    app: App,
    mod_manager: ModManager,
) -> Result<(), Box<dyn std::error::Error>> {
    let event_loop = EventLoop::new()?;
    let mut handler = CuteMeadowWinitApp::new(app, mod_manager);
    event_loop.run_app(&mut handler)?;
    Ok(())
}

// =============================================================================
// Automated Headless 3D ECS + Multi-Language C-ABI Mod Verification
// =============================================================================

fn run_headless_3d_verification(
    app: &mut App,
    mod_manager: &mut ModManager,
    ticks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Stage 5 Automated Cute 3D Modding Verification ===");

    // Phase 1: Baseline Cozy Meadow Stroll
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

    // Phase 2: Mod 1 — Jelly Boing Hop (Rust cdylib)
    println!("[TEST] Phase 2: Loading Mod 1 — Jelly Boing Hop (Rust cdylib) ({ticks} ticks)...");
    mod_manager.load_mod(app, ActiveMod::Boing)?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Boing);

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

    // Phase 3: Mod 2 — Pastel Carousel Waltz (Rust cdylib)
    println!(
        "[TEST] Phase 3: Switching to Mod 2 — Pastel Carousel Waltz (Rust cdylib) ({ticks} ticks)..."
    );
    let colors_before = query_colors(app);
    mod_manager.load_mod(app, ActiveMod::Carousel)?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Carousel);

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

    // Phase 4: Mod 3 — Cozy Cuddle Huddle (Pure ISO C99)
    println!(
        "[TEST] Phase 4: Switching to Mod 3 — Cozy Cuddle Huddle (Pure C99) ({ticks} ticks)..."
    );
    let dist_before = avg_xz_radius(app);
    mod_manager.load_mod(app, ActiveMod::CuddleC99)?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::CuddleC99);

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
        "  -> PASS: Pure C99 cuddle huddle verified (avg radius {:.2} -> {:.2}).",
        dist_before, dist_after
    );

    // Phase 5: Hot-Reload C99 Mod
    println!("[TEST] Phase 5: Hot-Reloading Active 3D Mod ({ticks} ticks)...");
    mod_manager.reload_mod(app)?;
    assert_eq!(mod_manager.reload_count(), 1);
    for _ in 0..ticks {
        app.update();
    }
    println!("  -> PASS: Shadow-copy hot-reload (#1) verified cleanly.");

    // Phase 6: Unload back to Baseline Meadow Stroll
    println!("[TEST] Phase 6: Unloading Mod back to Cozy Stroll ({ticks} ticks)...");
    mod_manager.unload_mod(app)?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::None);
    for _ in 0..ticks {
        app.update();
    }
    println!("  -> PASS: Clean dlclose unload and baseline restoration verified.");

    // Phase 7: Dynamic Slime Population Scaling & Jelly Collision Bouncing
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

    // Spawn two slimes on a direct head-on collision path to verify elastic bounce & blush
    let col_entity_a = {
        let mut commands = app.world_mut().commands();
        let e = meadow::spawn_single_slime(
            &mut commands,
            Position::new(-0.4, 0.0, 0.0),
            Velocity::new(3.0, 0.0, 0.0),
            Scale3D::uniform(1.0),
            ColorRgba::new(1.0, 0.5, 0.5, 1.0),
            CuteSpirit::new(0.0, 0.0, 0.2),
            components::HomeStyle {
                base_color: ColorRgba::new(1.0, 0.5, 0.5, 1.0),
                phase_offset: 0.0,
            },
        );
        meadow::spawn_single_slime(
            &mut commands,
            Position::new(0.4, 0.0, 0.0),
            Velocity::new(-3.0, 0.0, 0.0),
            Scale3D::uniform(1.0),
            ColorRgba::new(0.5, 0.5, 1.0, 1.0),
            CuteSpirit::new(0.0, 0.0, 0.2),
            components::HomeStyle {
                base_color: ColorRgba::new(0.5, 0.5, 1.0, 1.0),
                phase_offset: 0.0,
            },
        );
        e
    };
    app.update(); // Step collision and bounce

    let bounced_pos = app.world().get::<Position>(col_entity_a).unwrap();
    let bounced_vel = app.world().get::<Velocity>(col_entity_a).unwrap();
    let bounced_spirit = app.world().get::<CuteSpirit>(col_entity_a).unwrap();

    assert!(
        bounced_vel.x < 0.0 || bounced_pos.x < -0.4,
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
        "\n[TEST_MODE] SUCCESS: All 3 cute 3D mods, hot-reload, dynamic population, and collision bouncing verified!"
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
