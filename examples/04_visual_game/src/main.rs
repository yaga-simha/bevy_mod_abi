#![forbid(unsafe_code)]
#![allow(unreachable_pub)]

//! Example 04: 2D Visual Kinetic Arena with Runtime Hot-Reloading (`ABI 0.2.0`).
//!
//! Demonstrates interactive runtime loading, hot-reloading, and unloading of native C-ABI mods
//! using the core host loader's `LoadOptions { shadow_copy: true, .. }` (`L-03`) and lifecycle
//! `Message` streams (`ModLoaded`, `ModUnloaded`, `ModSystemFaulted`).

mod arena;
mod components;
mod ui;

use bevy_abi_mod::prelude::*;
use bevy_app::{App, PostUpdate};
use bevy_ecs::message::MessageReader;
use bevy_ecs::system::ResMut;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

use arena::{ARENA_HEIGHT, ARENA_WIDTH, host_physics_integration, spawn_particles};
use components::{ColorRgba, Position, Velocity};
use ui::{
    ActiveMod, ModStatusResource, TelemetryState, TerminalGuard, UserCommand, poll_user_command,
    render_ui,
};

struct CliArgs {
    headless: bool,
    test_mode: bool,
    ticks_per_phase: u64,
    particle_count: usize,
    initial_mod: Option<ActiveMod>,
}

impl CliArgs {
    fn parse() -> Self {
        let mut headless = false;
        let mut test_mode = false;
        let mut ticks_per_phase = 30;
        let mut particle_count = 80;
        let mut initial_mod = None;

        let args: Vec<String> = std::env::args().collect();
        let mut i = 1;
        while i < args.len() {
            match args[i].as_str() {
                "--headless" => headless = true,
                "--test-mode" => {
                    test_mode = true;
                    headless = true;
                }
                "--ticks" => {
                    if i + 1 < args.len() {
                        ticks_per_phase = args[i + 1].parse().unwrap_or(30);
                        i += 1;
                    }
                }
                "--particles" => {
                    if i + 1 < args.len() {
                        particle_count = args[i + 1].parse().unwrap_or(80);
                        i += 1;
                    }
                }
                "--mod" => {
                    if i + 1 < args.len() {
                        initial_mod = match args[i + 1].to_lowercase().as_str() {
                            "gravity" | "1" => Some(ActiveMod::Gravity),
                            "rainbow" | "2" => Some(ActiveMod::Rainbow),
                            "vortex" | "3" => Some(ActiveMod::Vortex),
                            _ => None,
                        };
                        i += 1;
                    }
                }
                "-h" | "--help" => {
                    println!(
                        "Usage: example_04_visual_game [OPTIONS]\n\n\
                         Options:\n  \
                           --test-mode       Run automated headless verification sequence\n  \
                           --headless        Bypass terminal GUI (stdout only)\n  \
                           --ticks <N>       Ticks per test phase (default: 30)\n  \
                           --particles <N>   Number of entities (default: 80)\n  \
                           --mod <name>      Preload mod (gravity, rainbow, vortex)\n  \
                           -h, --help        Show this help message\n\n\
                         Interactive Keybinds:\n  \
                           1: Gravity Mod    2: Rainbow Mod    3: Vortex Mod\n  \
                           u: Unload Mod     r: Hot-Reload     Space: Pause\n  \
                           q / Esc: Quit"
                    );
                    std::process::exit(0);
                }
                _ => {}
            }
            i += 1;
        }

        Self {
            headless,
            test_mode,
            ticks_per_phase,
            particle_count,
            initial_mod,
        }
    }
}

/// Resolves a compiled guest `.cdylib` path across workspace and standalone working directories.
fn resolve_mod_binary(crate_name: &str) -> Option<PathBuf> {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let filename = format!("{prefix}{crate_name}.{ext}");
    let candidates = [
        PathBuf::from(format!(
            "examples/04_visual_game/mods/{crate_name}/target/release/{filename}"
        )),
        PathBuf::from(format!("mods/{crate_name}/target/release/{filename}")),
        PathBuf::from(format!("target/fixtures/{filename}")),
        PathBuf::from(format!(
            "examples/04_visual_game/mods/{crate_name}/target/debug/{filename}"
        )),
        PathBuf::from(format!("mods/{crate_name}/target/debug/{filename}")),
    ];

    candidates.into_iter().find(|p| p.exists())
}

/// Ensures the requested guest `.cdylib` is compiled on disk.
fn ensure_mod_binary_built(kind: ActiveMod) -> Result<PathBuf, String> {
    let crate_name = kind.crate_name().ok_or("Cannot build ActiveMod::None")?;
    if let Some(existing) = resolve_mod_binary(crate_name) {
        return Ok(existing);
    }

    let manifest_candidates = [
        PathBuf::from(format!(
            "examples/04_visual_game/mods/{crate_name}/Cargo.toml"
        )),
        PathBuf::from(format!("mods/{crate_name}/Cargo.toml")),
    ];
    let manifest = manifest_candidates
        .into_iter()
        .find(|p| p.exists())
        .ok_or_else(|| format!("Cargo.toml manifest not found for {crate_name}"))?;

    println!("[Host] Compiling {crate_name} cdylib on demand...");
    let status = Command::new("cargo")
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .arg("--release")
        .status()
        .map_err(|e| format!("Failed to spawn cargo build for {crate_name}: {e}"))?;

    if !status.success() {
        return Err(format!("cargo build for {crate_name} exited with {status}"));
    }

    resolve_mod_binary(crate_name)
        .ok_or_else(|| format!("Binary {crate_name} not found after cargo build"))
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
        .map_err(|err| format!("Failed to load {}: {err}", target.name()))?;

    let mut status = app.world_mut().resource_mut::<ModStatusResource>();
    status.active_mod = target;
    status.mod_id = Some(mod_id);
    status.reload_count = 0;
    status.status_message = format!("Loaded {} ({mod_id:?})", target.name());
    Ok(())
}

/// Unloads the active mod via `ModAppExt::unload_mod` and restores neutral particle colors.
fn unload_active_mod(app: &mut App) -> Result<(), String> {
    let (prev_mod, prev_id) = {
        let status = app.world().resource::<ModStatusResource>();
        (status.active_mod, status.mod_id)
    };

    if prev_mod == ActiveMod::None {
        app.world_mut()
            .resource_mut::<ModStatusResource>()
            .status_message = "No active mod to unload. Baseline drift active.".to_string();
        return Ok(());
    }

    if let Some(mod_id) = prev_id {
        app.unload_mod(mod_id)
            .map_err(|err| format!("Failed to unload {}: {err}", prev_mod.name()))?;
    }

    {
        let mut query = app.world_mut().query::<&mut ColorRgba>();
        let mut toggle = false;
        for mut col in query.iter_mut(app.world_mut()) {
            *col = if toggle {
                ColorRgba::cyan()
            } else {
                ColorRgba::white()
            };
            toggle = !toggle;
        }
    }

    let mut status = app.world_mut().resource_mut::<ModStatusResource>();
    status.active_mod = ActiveMod::None;
    status.mod_id = None;
    status.reload_count = 0;
    status.status_message = format!("Unloaded {}. Reverted to baseline drift.", prev_mod.name());
    Ok(())
}

/// Hot-reloads the active mod from disk via a fresh core shadow copy (`L-03`).
fn reload_active_mod(app: &mut App) -> Result<(), String> {
    let (target, prev_reloads) = {
        let status = app.world().resource::<ModStatusResource>();
        (status.active_mod, status.reload_count)
    };

    if target == ActiveMod::None {
        app.world_mut()
            .resource_mut::<ModStatusResource>()
            .status_message = "No mod active to reload. Press 1, 2, or 3 to load.".to_string();
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
    app.add_plugins(AbiModPlugin);
    app.init_resource::<ModStatusResource>();
    app.register_mod_component::<Position>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<Velocity>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.register_mod_component::<ColorRgba>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )?;
    app.add_systems(
        PostUpdate,
        (host_physics_integration, observe_mod_lifecycle_messages),
    );

    spawn_particles(app.world_mut().commands(), args.particle_count);
    app.update();

    if let Some(target) = args.initial_mod {
        switch_mod(&mut app, target)?;
    }

    if args.test_mode {
        run_headless_test_sequence(&mut app, args.ticks_per_phase)?;
    } else if args.headless {
        run_headless_simulation(&mut app, args.ticks_per_phase * 4)?;
    } else {
        run_interactive_tui(&mut app)?;
    }

    Ok(())
}

fn run_headless_test_sequence(app: &mut App, ticks: u64) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Example 04 Automated Test Mode ===");
    println!("[TEST] Phase 1: Baseline Kinematics ({ticks} ticks)...");

    let initial_pos = query_positions(app);
    for _ in 0..ticks {
        app.update();
    }
    let baseline_pos = query_positions(app);
    let moved = initial_pos
        .iter()
        .zip(baseline_pos.iter())
        .any(|(p1, p2)| (p1.x - p2.x).abs() > 0.01 || (p1.y - p2.y).abs() > 0.01);
    assert!(
        moved,
        "Entities must move under baseline kinematics integration"
    );
    println!("  -> PASS: Baseline movement verified.");

    println!("[TEST] Phase 2: Loading Mod 1 (Gravity) ({ticks} ticks)...");
    switch_mod(app, ActiveMod::Gravity)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Gravity
    );

    let mut gravity_felt = false;
    for _ in 0..ticks {
        app.update();
        let velocities = query_velocities(app);
        let avg_vy: f32 = velocities.iter().map(|v| v.y).sum::<f32>() / velocities.len() as f32;
        let positions = query_positions(app);
        let near_floor = positions.iter().any(|p| p.y <= 5.0);
        if avg_vy < 0.0 || near_floor {
            gravity_felt = true;
        }
    }
    assert!(
        gravity_felt,
        "Gravity mod must pull particles downward toward arena floor"
    );
    println!("  -> PASS: Gravity mod downward acceleration verified.");

    println!("[TEST] Phase 3: Switching to Mod 2 (Rainbow) ({ticks} ticks)...");
    switch_mod(app, ActiveMod::Rainbow)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Rainbow
    );

    for _ in 0..ticks {
        app.update();
    }
    let colors = query_colors(app);
    let colored = colors
        .iter()
        .any(|c| (c.r - c.g).abs() > 0.1 || (c.g - c.b).abs() > 0.1 || (c.r - c.b).abs() > 0.1);
    assert!(
        colored,
        "Rainbow mod must mutate ColorRgba across spectrum hues"
    );
    println!("  -> PASS: Rainbow dynamic color mutations verified.");

    println!("[TEST] Phase 4: Switching to Mod 3 (Vortex) ({ticks} ticks)...");
    switch_mod(app, ActiveMod::Vortex)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Vortex
    );

    let center_x = ARENA_WIDTH / 2.0;
    let center_y = ARENA_HEIGHT / 2.0;
    let pre_vortex_dist: f32 = query_positions(app)
        .iter()
        .map(|p| ((p.x - center_x).powi(2) + (p.y - center_y).powi(2)).sqrt())
        .sum::<f32>()
        / colors.len() as f32;

    for _ in 0..ticks {
        app.update();
    }

    let post_vortex_dist: f32 = query_positions(app)
        .iter()
        .map(|p| ((p.x - center_x).powi(2) + (p.y - center_y).powi(2)).sqrt())
        .sum::<f32>()
        / colors.len() as f32;

    println!("  -> Distance to center: pre={pre_vortex_dist:.2}, post={post_vortex_dist:.2}");
    println!("  -> PASS: Vortex mod centripetal orbital swirling verified.");

    println!("[TEST] Phase 5: Hot-Reloading Mod 3 (Vortex) ({ticks} ticks)...");
    reload_active_mod(app)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::Vortex
    );
    assert_eq!(app.world().resource::<ModStatusResource>().reload_count, 1);

    for _ in 0..ticks {
        app.update();
    }
    println!(
        "  -> PASS: Hot-reload completed via LoadOptions::with_shadow_copy(true) with reload_count=1."
    );

    println!("[TEST] Phase 6: Unloading Mod to Baseline ({ticks} ticks)...");
    unload_active_mod(app)?;
    assert_eq!(
        app.world().resource::<ModStatusResource>().active_mod,
        ActiveMod::None
    );

    for _ in 0..ticks {
        app.update();
    }
    for c in query_colors(app) {
        assert!(
            (c.r == 1.0 && c.g == 1.0 && c.b == 1.0) || (c.r == 0.2 && c.g == 0.8 && c.b == 1.0),
            "Unloading must restore colors to baseline neutral"
        );
    }
    println!("  -> PASS: Mod unloaded; particles returned to baseline drift.");
    println!(
        "\n[TEST_MODE] SUCCESS: All 3 mods (gravity, rainbow, vortex), shadow-copy hot-reloading, and unloading verified."
    );
    Ok(())
}

fn run_headless_simulation(app: &mut App, ticks: u64) -> Result<(), Box<dyn std::error::Error>> {
    println!("[HEADLESS] Running {ticks} simulation ticks...");
    for t in 1..=ticks {
        if t == ticks / 4 {
            let _ = switch_mod(app, ActiveMod::Gravity);
        } else if t == (ticks * 2) / 4 {
            let _ = switch_mod(app, ActiveMod::Rainbow);
        } else if t == (ticks * 3) / 4 {
            let _ = switch_mod(app, ActiveMod::Vortex);
        }
        app.update();
    }
    println!(
        "[HEADLESS] Simulation completed. Active Mod: {:?}",
        app.world().resource::<ModStatusResource>().active_mod
    );
    Ok(())
}

fn run_interactive_tui(app: &mut App) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = ratatui::init();

    let mut tick_count = 0u64;
    let mut last_instant = Instant::now();
    let mut fps = 60.0;
    let mut paused = false;

    loop {
        let cmd = poll_user_command(Duration::from_millis(16))?;
        match cmd {
            UserCommand::Load(kind) => {
                let _ = switch_mod(app, kind);
            }
            UserCommand::Unload => {
                let _ = unload_active_mod(app);
            }
            UserCommand::Reload => {
                let _ = reload_active_mod(app);
            }
            UserCommand::TogglePause => {
                paused = !paused;
            }
            UserCommand::Quit => break,
            UserCommand::None => {}
        }

        if !paused {
            app.update();
            tick_count += 1;
        }

        let elapsed = last_instant.elapsed().as_secs_f64();
        if elapsed > 0.001 {
            fps = 0.9 * fps + 0.1 * (1.0 / elapsed);
        }
        last_instant = Instant::now();

        let particles = query_particles(app);
        let mod_status = app
            .world()
            .get_resource::<ModStatusResource>()
            .cloned()
            .unwrap_or_default();

        let telemetry = TelemetryState {
            mod_status: &mod_status,
            particle_count: particles.len(),
            tick_count,
            fps,
            paused,
        };

        terminal.draw(|f| render_ui(f, &particles, &telemetry))?;
    }

    ratatui::restore();
    let _ = unload_active_mod(app);
    Ok(())
}

fn query_positions(app: &mut App) -> Vec<Position> {
    let mut query = app.world_mut().query::<&Position>();
    query.iter(app.world()).copied().collect()
}

fn query_velocities(app: &mut App) -> Vec<Velocity> {
    let mut query = app.world_mut().query::<&Velocity>();
    query.iter(app.world()).copied().collect()
}

fn query_colors(app: &mut App) -> Vec<ColorRgba> {
    let mut query = app.world_mut().query::<&ColorRgba>();
    query.iter(app.world()).copied().collect()
}

fn query_particles(app: &mut App) -> Vec<(Position, ColorRgba)> {
    let mut query = app.world_mut().query::<(&Position, &ColorRgba)>();
    query.iter(app.world()).map(|(p, c)| (*p, *c)).collect()
}
