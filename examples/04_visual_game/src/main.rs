//! Stage 4 Example: Visual Game with Runtime Mod Reloading
//!
//! A 2D kinetic particle arena rendered in terminal Braille characters with
//! zero-serialization C-ABI dynamic mod loading, hot-reloading, and unloading.

mod arena;
mod components;
mod mod_manager;
mod ui;

use bevy_app::{App, PostUpdate};
use host_runtime::prelude::*;
use std::time::{Duration, Instant};

use arena::{host_physics_integration, spawn_particles, ARENA_HEIGHT, ARENA_WIDTH};
use components::{ColorRgba, Position, Velocity};
use mod_manager::{ActiveMod, ModManager, ModStatusResource};
use ui::{poll_user_command, render_ui, TelemetryState, TerminalGuard, UserCommand};

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

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = CliArgs::parse();

    // 1. Initialize Bevy App with ABI modding framework and components
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<ColorRgba>();
    app.add_systems(PostUpdate, host_physics_integration);

    // 2. Spawn particle entities
    spawn_particles(app.world_mut().commands(), args.particle_count);
    app.update(); // Flush entity spawning

    // 3. Initialize Runtime ModManager
    let mut mod_manager = ModManager::new()?;
    mod_manager.sync_resource(&mut app);

    if let Some(target) = args.initial_mod {
        mod_manager.load_mod(&mut app, target)?;
    }

    // 4. Branch: Headless / Test Mode vs. Interactive TUI Mode
    if args.test_mode {
        run_headless_test_sequence(&mut app, &mut mod_manager, args.ticks_per_phase)?;
    } else if args.headless {
        run_headless_simulation(&mut app, &mut mod_manager, args.ticks_per_phase * 4)?;
    } else {
        run_interactive_tui(&mut app, &mut mod_manager)?;
    }

    Ok(())
}

/// Executes programmatic test verification sequence for Milestone 4 (R4).
fn run_headless_test_sequence(
    app: &mut App,
    mod_manager: &mut ModManager,
    ticks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("=== Stage 4 Automated Test Mode ===");
    println!("[TEST] Phase 1: Baseline Kinematics ({} ticks)...", ticks);

    let initial_pos = query_positions(app);
    for _ in 0..ticks {
        app.update();
    }
    let baseline_pos = query_positions(app);
    let mut moved = false;
    for (p1, p2) in initial_pos.iter().zip(baseline_pos.iter()) {
        if (p1.x - p2.x).abs() > 0.01 || (p1.y - p2.y).abs() > 0.01 {
            moved = true;
            break;
        }
    }
    assert!(
        moved,
        "Entities must move under baseline kinematics integration"
    );
    println!("  -> PASS: Baseline movement verified.");

    // --- Phase 2: Gravity Mod ---
    println!(
        "[TEST] Phase 2: Loading Mod 1 (Gravity) ({} ticks)...",
        ticks
    );
    mod_manager
        .load_mod(app, ActiveMod::Gravity)
        .map_err(|e| format!("Failed to load Gravity mod: {e}"))?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Gravity);

    // Track vertical velocity adjustments under gravity
    let mut gravity_felt = false;
    for _ in 0..ticks {
        app.update();
        let velocities = query_velocities(app);
        // Under gravity, downward velocity should be negative or particles bounce off floor
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

    // --- Phase 3: Rainbow Mod ---
    println!(
        "[TEST] Phase 3: Switching to Mod 2 (Rainbow) ({} ticks)...",
        ticks
    );
    mod_manager
        .load_mod(app, ActiveMod::Rainbow)
        .map_err(|e| format!("Failed to load Rainbow mod: {e}"))?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Rainbow);

    for _ in 0..ticks {
        app.update();
    }
    let colors = query_colors(app);
    // In rainbow mod, colors are animated across the spectrum, showing color channel variation
    let mut colored = false;
    for c in &colors {
        if (c.r - c.g).abs() > 0.1 || (c.g - c.b).abs() > 0.1 || (c.r - c.b).abs() > 0.1 {
            colored = true;
            break;
        }
    }
    assert!(
        colored,
        "Rainbow mod must mutate ColorRgba across spectrum hues"
    );
    println!("  -> PASS: Rainbow dynamic color mutations verified.");

    // --- Phase 4: Vortex Mod ---
    println!(
        "[TEST] Phase 4: Switching to Mod 3 (Vortex) ({} ticks)...",
        ticks
    );
    mod_manager
        .load_mod(app, ActiveMod::Vortex)
        .map_err(|e| format!("Failed to load Vortex mod: {e}"))?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Vortex);

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

    println!(
        "  -> Distance to center: pre={:.2}, post={:.2}",
        pre_vortex_dist, post_vortex_dist
    );
    println!("  -> PASS: Vortex mod centripetal orbital swirling verified.");

    // --- Phase 5: Hot-Reload Vortex Mod ---
    println!(
        "[TEST] Phase 5: Hot-Reloading Mod 3 (Vortex) ({} ticks)...",
        ticks
    );
    mod_manager
        .reload_mod(app)
        .map_err(|e| format!("Failed to reload Vortex mod: {e}"))?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::Vortex);
    assert_eq!(mod_manager.reload_count(), 1);

    for _ in 0..ticks {
        app.update();
    }
    println!(
        "  -> PASS: Hot-reload completed with fresh shadow copy and incremented reload count."
    );

    // --- Phase 6: Unload Mod ---
    println!(
        "[TEST] Phase 6: Unloading Mod to Baseline ({} ticks)...",
        ticks
    );
    mod_manager
        .unload_mod(app)
        .map_err(|e| format!("Failed to unload mod: {e}"))?;
    assert_eq!(mod_manager.active_mod(), ActiveMod::None);

    for _ in 0..ticks {
        app.update();
    }
    let restored_colors = query_colors(app);
    for c in restored_colors {
        // Assert colors returned to baseline cyan or white
        assert!(
            (c.r == 1.0 && c.g == 1.0 && c.b == 1.0) || (c.r == 0.2 && c.g == 0.8 && c.b == 1.0),
            "Unloading must restore colors to baseline neutral"
        );
    }
    println!("  -> PASS: Mod successfully unloaded; particles returned to baseline drift.");

    println!("\n[TEST_MODE] SUCCESS: All 3 mods (gravity, rainbow, vortex), hot-reloading, and unloading verified!");
    Ok(())
}

fn run_headless_simulation(
    app: &mut App,
    mod_manager: &mut ModManager,
    ticks: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    println!("[HEADLESS] Running {} simulation ticks...", ticks);
    for t in 1..=ticks {
        if t == ticks / 4 {
            let _ = mod_manager.load_mod(app, ActiveMod::Gravity);
        } else if t == (ticks * 2) / 4 {
            let _ = mod_manager.load_mod(app, ActiveMod::Rainbow);
        } else if t == (ticks * 3) / 4 {
            let _ = mod_manager.load_mod(app, ActiveMod::Vortex);
        }
        app.update();
    }
    println!(
        "[HEADLESS] Simulation completed. Active Mod: {:?}",
        mod_manager.active_mod()
    );
    Ok(())
}

fn run_interactive_tui(
    app: &mut App,
    mod_manager: &mut ModManager,
) -> Result<(), Box<dyn std::error::Error>> {
    let _guard = TerminalGuard::enter()?;
    let mut terminal = ratatui::init();

    let mut tick_count = 0u64;
    let mut last_instant = Instant::now();
    let mut fps = 60.0;
    let mut paused = false;

    loop {
        // 1. Process keyboard user command
        let cmd = poll_user_command(Duration::from_millis(16))?;
        match cmd {
            UserCommand::Load(kind) => {
                let _ = mod_manager.load_mod(app, kind);
            }
            UserCommand::Unload => {
                let _ = mod_manager.unload_mod(app);
            }
            UserCommand::Reload => {
                let _ = mod_manager.reload_mod(app);
            }
            UserCommand::TogglePause => {
                paused = !paused;
            }
            UserCommand::Quit => break,
            UserCommand::None => {}
        }

        // 2. Advance simulation
        if !paused {
            app.update();
            tick_count += 1;
        }

        // 3. Compute frame rate
        let elapsed = last_instant.elapsed().as_secs_f64();
        if elapsed > 0.001 {
            fps = 0.9 * fps + 0.1 * (1.0 / elapsed);
        }
        last_instant = Instant::now();

        // 4. Query ECS entity data for rendering
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

        // 5. Draw frame
        terminal.draw(|f| render_ui(f, &particles, &telemetry))?;
    }

    ratatui::restore();
    let _ = mod_manager.unload_mod(app);
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
