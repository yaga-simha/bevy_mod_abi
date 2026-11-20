//! Terminal UI rendering using Ratatui and Crossterm.
//!
//! Provides the Braille Canvas widget, telemetry HUD, keyboard command polling,
//! and RAII terminal restoration guard.

use bevy_ecs::prelude::Resource;
use bevy_mod_abi::ModId;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        Block, Paragraph,
        canvas::{Canvas, Points},
    },
};
use std::io::stdout;
use std::time::Duration;

use crate::arena::{ARENA_HEIGHT, ARENA_WIDTH};
use crate::components::{ColorRgba, Position};

/// Active mod slot selection in the Kinetic Arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ActiveMod {
    /// Baseline drift with no active guest mod.
    #[default]
    None,
    /// Downward gravitational acceleration and floor bouncing (`mod_gravity`).
    Gravity,
    /// Animated HSV rainbow wave (`mod_rainbow`).
    Rainbow,
    /// Centripetal orbital vortex (`mod_vortex`).
    Vortex,
}

impl ActiveMod {
    /// Human-readable display label for the HUD.
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "None (Baseline Drift)",
            Self::Gravity => "Gravity (Kinetic Bouncer)",
            Self::Rainbow => "Rainbow (Color Spectrum)",
            Self::Vortex => "Vortex (Orbital Swirl)",
        }
    }

    /// Guest crate stem on disk (`None` when unloaded).
    pub const fn crate_name(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Gravity => Some("mod_gravity"),
            Self::Rainbow => Some("mod_rainbow"),
            Self::Vortex => Some("mod_vortex"),
        }
    }
}

/// ECS [`Resource`] tracking the active mod slot and lifecycle status for HUD rendering.
#[derive(Resource, Debug, Clone)]
pub struct ModStatusResource {
    /// Currently active mod slot.
    pub active_mod: ActiveMod,
    /// Loaded [`ModId`] assigned by the host loader, if any.
    pub mod_id: Option<ModId>,
    /// Number of hot-reloads performed on the current mod slot.
    pub reload_count: u32,
    /// Cumulative system faults reported via `ModSystemFaulted`.
    pub fault_count: usize,
    /// Latest status message shown in the HUD.
    pub status_message: String,
}

impl Default for ModStatusResource {
    fn default() -> Self {
        Self {
            active_mod: ActiveMod::None,
            mod_id: None,
            reload_count: 0,
            fault_count: 0,
            status_message: "Ready. Entities in baseline drift.".to_string(),
        }
    }
}

/// RAII Terminal Guard ensuring terminal raw mode and alternate screen
/// are restored on exit or panic.
pub struct TerminalGuard;

impl TerminalGuard {
    /// Enters raw mode and alternate screen, installing a panic hook for cleanup.
    pub fn enter() -> Result<Self, std::io::Error> {
        enable_raw_mode()?;
        execute!(stdout(), EnterAlternateScreen, cursor::Hide)?;

        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |panic_info| {
            let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
            let _ = disable_raw_mode();
            default_hook(panic_info);
        }));

        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(stdout(), LeaveAlternateScreen, cursor::Show);
        let _ = disable_raw_mode();
    }
}

/// User input commands recognized by the interactive TUI loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCommand {
    /// Load or switch to `ActiveMod`.
    Load(ActiveMod),
    /// Unload active mod.
    Unload,
    /// Hot-reload active mod via core shadow-copy loader.
    Reload,
    /// Pause or resume simulation ticks.
    TogglePause,
    /// Exit application.
    Quit,
    /// No input event within poll window.
    None,
}

/// Polls for user keyboard commands with non-blocking timeout.
pub fn poll_user_command(timeout: Duration) -> Result<UserCommand, std::io::Error> {
    if event::poll(timeout)?
        && let Event::Key(key) = event::read()?
        && key.kind == KeyEventKind::Press
    {
        return Ok(match key.code {
            KeyCode::Char('1') => UserCommand::Load(ActiveMod::Gravity),
            KeyCode::Char('2') => UserCommand::Load(ActiveMod::Rainbow),
            KeyCode::Char('3') => UserCommand::Load(ActiveMod::Vortex),
            KeyCode::Char('u') | KeyCode::Char('U') => UserCommand::Unload,
            KeyCode::Char('r') | KeyCode::Char('R') => UserCommand::Reload,
            KeyCode::Char(' ') => UserCommand::TogglePause,
            KeyCode::Char('q') | KeyCode::Char('Q') | KeyCode::Esc => UserCommand::Quit,
            _ => UserCommand::None,
        });
    }
    Ok(UserCommand::None)
}

/// Telemetry state snapshot passed to the TUI renderer.
pub struct TelemetryState<'a> {
    /// Current mod slot status.
    pub mod_status: &'a ModStatusResource,
    /// Active particle count.
    pub particle_count: usize,
    /// Simulation tick counter.
    pub tick_count: u64,
    /// Smoothed frames per second.
    pub fps: f64,
    /// Whether simulation stepping is paused.
    pub paused: bool,
}

/// Renders the frame: Canvas particle arena and HUD telemetry panel.
pub fn render_ui(
    frame: &mut Frame,
    particles: &[(Position, ColorRgba)],
    telemetry: &TelemetryState<'_>,
) {
    let vertical = Layout::vertical([Constraint::Min(12), Constraint::Length(5)]);
    let [arena_area, hud_area] = vertical.areas(frame.area());

    let canvas = Canvas::default()
        .block(Block::bordered().title(" 2D Kinetic Arena (bevy_abi_mod v0.2) "))
        .marker(Marker::Braille)
        .x_bounds([0.0, ARENA_WIDTH as f64])
        .y_bounds([0.0, ARENA_HEIGHT as f64])
        .paint(|ctx| {
            for (pos, col) in particles {
                let color = Color::Rgb(
                    (col.r.clamp(0.0, 1.0) * 255.0) as u8,
                    (col.g.clamp(0.0, 1.0) * 255.0) as u8,
                    (col.b.clamp(0.0, 1.0) * 255.0) as u8,
                );
                ctx.draw(&Points {
                    coords: &[(pos.x as f64, pos.y as f64)],
                    color,
                });
            }
        });
    frame.render_widget(canvas, arena_area);

    let mod_name = telemetry.mod_status.active_mod.name();
    let mod_badge_style = match telemetry.mod_status.active_mod {
        ActiveMod::Gravity => Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
        ActiveMod::Rainbow => Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD),
        ActiveMod::Vortex => Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
        ActiveMod::None => Style::default().fg(Color::DarkGray),
    };

    let line1 = Line::from(vec![
        Span::raw("Active Mod: "),
        Span::styled(format!("[{mod_name}]"), mod_badge_style),
        Span::raw(format!(
            " │ Particles: {} │ Ticks: {} │ {:.1} FPS {}",
            telemetry.particle_count,
            telemetry.tick_count,
            telemetry.fps,
            if telemetry.paused { "(PAUSED)" } else { "" }
        )),
    ]);

    let line2 = Line::from(format!(
        "Status: {} │ Reloads: {} │ Faults: {}",
        telemetry.mod_status.status_message,
        telemetry.mod_status.reload_count,
        telemetry.mod_status.fault_count,
    ));

    let line3 = Line::from(
        "Controls: [1] Gravity  [2] Rainbow  [3] Vortex  [U] Unload  [R] Reload  [Space] Pause  [Q] Quit",
    );

    let hud = Paragraph::new(vec![line1, line2, line3])
        .block(Block::bordered().title(" Engine & Mod Telemetry "))
        .style(Style::default().fg(Color::White));
    frame.render_widget(hud, hud_area);
}
