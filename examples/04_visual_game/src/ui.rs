//! Terminal UI rendering using Ratatui and Crossterm.
//!
//! Provides the sub-pixel Braille Canvas widget, telemetry HUD,
//! keyboard command polling, and RAII terminal protection.

use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    symbols::Marker,
    text::{Line, Span},
    widgets::{
        canvas::{Canvas, Points},
        Block, Paragraph,
    },
    Frame,
};
use std::io::stdout;
use std::time::Duration;

use crate::arena::{ARENA_HEIGHT, ARENA_WIDTH};
use crate::components::{ColorRgba, Position};
use crate::mod_manager::{ActiveMod, ModStatusResource};

/// RAII Terminal Guard ensuring terminal raw mode and alternate screen
/// are always safely restored on exit or panic.
pub struct TerminalGuard;

impl TerminalGuard {
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

/// User input commands recognized by the game loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCommand {
    Load(ActiveMod),
    Unload,
    Reload,
    TogglePause,
    Quit,
    None,
}

/// Polls for user keyboard commands with non-blocking timeout.
pub fn poll_user_command(timeout: Duration) -> Result<UserCommand, std::io::Error> {
    if event::poll(timeout)? {
        if let Event::Key(key) = event::read()? {
            if key.kind == KeyEventKind::Press {
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
        }
    }
    Ok(UserCommand::None)
}

/// Telemetry state snapshot passed to renderer.
pub struct TelemetryState<'a> {
    pub mod_status: &'a ModStatusResource,
    pub particle_count: usize,
    pub tick_count: u64,
    pub fps: f64,
    pub paused: bool,
}

/// Renders the frame: Canvas particle arena and HUD telemetry panel.
pub fn render_ui(
    frame: &mut Frame,
    particles: &[(Position, ColorRgba)],
    telemetry: &TelemetryState,
) {
    let vertical = Layout::vertical([Constraint::Min(12), Constraint::Length(5)]);
    let [arena_area, hud_area] = vertical.areas(frame.area());

    // 1. Render Arena Canvas with Sub-Pixel Braille Resolution
    let canvas = Canvas::default()
        .block(Block::bordered().title(" 2D Kinetic Arena (bevy_abi_mod) "))
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

    // 2. Render Telemetry HUD
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
        "Status: {} │ Reloads: {}",
        telemetry.mod_status.status_message, telemetry.mod_status.reload_count,
    ));

    let line3 = Line::from(
        "Controls: [1] Gravity  [2] Rainbow  [3] Vortex  [U] Unload  [R] Reload  [Space] Pause  [Q] Quit",
    );

    let hud = Paragraph::new(vec![line1, line2, line3])
        .block(Block::bordered().title(" Engine & Mod Telemetry "))
        .style(Style::default().fg(Color::White));
    frame.render_widget(hud, hud_area);
}
