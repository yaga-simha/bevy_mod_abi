#![forbid(unsafe_code)]

//! Mod 2: Dynamic Rainbow Spectrum (`mod_rainbow`, `ABI 0.2.0`).
//!
//! Generates animated rainbow hues across entities from spatial position and a
//! per-World tick counter stored in the system closure.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;

/// Guest mirror of `example::Position`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
    /// Depth coordinate.
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static CStr = c"example::Position";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Guest mirror of `example::ColorRgba`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "example::ColorRgba")]
pub struct ColorRgba {
    /// Red channel in `[0.0, 1.0]`.
    pub r: f32,
    /// Green channel in `[0.0, 1.0]`.
    pub g: f32,
    /// Blue channel in `[0.0, 1.0]`.
    pub b: f32,
    /// Alpha channel in `[0.0, 1.0]`.
    pub a: f32,
}

impl GuestComponent for ColorRgba {
    const NAME: &'static CStr = c"example::ColorRgba";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[inline]
fn hsv_to_rgb(hue: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let c = v * s;
    let h_prime = (hue % 360.0) / 60.0;
    let x = c * (1.0 - ((h_prime % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if h_prime < 1.0 {
        (c, x, 0.0)
    } else if h_prime < 2.0 {
        (x, c, 0.0)
    } else if h_prime < 3.0 {
        (0.0, c, x)
    } else if h_prime < 4.0 {
        (0.0, x, c)
    } else if h_prime < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

/// Per-World guest plugin for `mod_rainbow`.
#[derive(Default)]
pub struct RainbowMod;

impl GuestPlugin for RainbowMod {
    const NAME: &'static CStr = c"mod_rainbow";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        let mut tick: u64 = 0;
        app.system(c"RainbowSpectrumSystem")
            .query(|q| q.write::<ColorRgba>().read::<Position>())
            .build(move |ctx| {
                let current_tick = tick;
                tick = tick.wrapping_add(1);

                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some((positions, colors)) =
                        chunk.component_read_write::<Position, ColorRgba>(1, 0)
                    else {
                        return FfiResult::TYPE_MISMATCH;
                    };

                    for (pos, col) in positions.iter().zip(colors.iter_mut()) {
                        let phase = ((pos.x / ARENA_WIDTH) * 1.5
                            + (pos.y / ARENA_HEIGHT)
                            + (current_tick as f32 * 0.03))
                            % 1.0;
                        let hue = (phase.abs() * 360.0) % 360.0;
                        let (r, g, b) = hsv_to_rgb(hue, 1.0, 1.0);
                        col.r = r;
                        col.g = g;
                        col.b = b;
                        col.a = 1.0;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: RainbowMod);
