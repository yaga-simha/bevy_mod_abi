#![forbid(unsafe_code)]

//! Mod 2: Pastel Starlight Carousel (`mod_carousel`, `ABI 0.2.0`).
//!
//! Guides slime spirits onto two counter-rotating 3D carousel rings with bobbing waves,
//! pastel hue cycling, and synchronized ear wiggles using per-World closure state.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

const DT: f32 = 1.0 / 60.0;

/// Guest mirror of `cute3d::Position`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "cute3d::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static CStr = c"cute3d::Position";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Guest mirror of `cute3d::Velocity`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "cute3d::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

impl GuestComponent for Velocity {
    const NAME: &'static CStr = c"cute3d::Velocity";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Guest mirror of `cute3d::Scale3D`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "cute3d::Scale3D")]
pub struct Scale3D {
    /// X scale factor.
    pub x: f32,
    /// Y scale factor.
    pub y: f32,
    /// Z scale factor.
    pub z: f32,
}

impl GuestComponent for Scale3D {
    const NAME: &'static CStr = c"cute3d::Scale3D";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Guest mirror of `cute3d::ColorRgba`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "cute3d::ColorRgba")]
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
    const NAME: &'static CStr = c"cute3d::ColorRgba";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Guest mirror of `cute3d::CuteSpirit`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "cute3d::CuteSpirit")]
pub struct CuteSpirit {
    /// Facing angle in radians.
    pub yaw: f32,
    /// Ear tilt angle in radians.
    pub ear_wiggle: f32,
    /// Cheek blush intensity in `[0.0, 1.0]`.
    pub blush: f32,
}

impl GuestComponent for CuteSpirit {
    const NAME: &'static CStr = c"cute3d::CuteSpirit";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[inline]
fn pastel_hsv_to_rgb(hue_deg: f32) -> (f32, f32, f32) {
    let s = 0.46_f32;
    let v = 0.98_f32;
    let c = v * s;
    let h = ((hue_deg % 360.0) + 360.0) % 360.0 / 60.0;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let (r1, g1, b1) = if h < 1.0 {
        (c, x, 0.0)
    } else if h < 2.0 {
        (x, c, 0.0)
    } else if h < 3.0 {
        (0.0, c, x)
    } else if h < 4.0 {
        (0.0, x, c)
    } else if h < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };
    let m = v - c;
    (r1 + m, g1 + m, b1 + m)
}

/// Per-World guest plugin for `mod_carousel`.
#[derive(Default)]
pub struct CarouselMod;

impl GuestPlugin for CarouselMod {
    const NAME: &'static CStr = c"mod_carousel";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        let mut tick: u64 = 0;
        app.system(c"StarlightCarouselSystem")
            .query(|q| {
                q.write::<Position>()
                    .write::<Velocity>()
                    .write::<Scale3D>()
                    .write::<ColorRgba>()
                    .write::<CuteSpirit>()
            })
            .build(move |ctx| {
                let current_tick = tick;
                tick = tick.wrapping_add(1);
                let t = (current_tick as f32) * DT;
                let mut global_base: usize = 0;

                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let count = chunk.len();

                    {
                        let Some((positions, velocities)) =
                            chunk.component_write_2::<Position, Velocity>(0, 1)
                        else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for (row, (pos, vel)) in
                            positions.iter_mut().zip(velocities.iter_mut()).enumerate()
                        {
                            let idx = global_base + row;
                            let outer_ring = idx.is_multiple_of(2);
                            let ring_radius = if outer_ring { 5.6 } else { 3.1 };
                            let angular_speed = if outer_ring { 0.85 } else { -1.15 };
                            let slot_angle =
                                (idx as f32) * core::f32::consts::FRAC_PI_6 + t * angular_speed;

                            let target_x = ring_radius * slot_angle.cos();
                            let target_z = ring_radius * slot_angle.sin();
                            let bob_phase = t * 3.6 + (idx as f32) * 0.9;
                            let target_y = 0.65 + 0.55 * bob_phase.sin();

                            let vx = (target_x - pos.x) * 3.8;
                            let vz = (target_z - pos.z) * 3.8;
                            let vy = (target_y - pos.y) * 5.0;

                            pos.y = (pos.y + vy * DT).max(0.0);
                            vel.x = vx;
                            vel.y = 0.0;
                            vel.z = vz;
                        }
                    }

                    {
                        let Some((scales, colors)) =
                            chunk.component_write_2::<Scale3D, ColorRgba>(2, 3)
                        else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for (row, (scale, color)) in
                            scales.iter_mut().zip(colors.iter_mut()).enumerate()
                        {
                            let idx = global_base + row;
                            let bob_phase = t * 3.6 + (idx as f32) * 0.9;
                            let pulse = 1.0 + 0.12 * (bob_phase * 2.0).sin();
                            let inv_pulse = 1.0 / pulse.sqrt();
                            *scale = Scale3D {
                                x: inv_pulse,
                                y: pulse,
                                z: inv_pulse,
                            };

                            let hue = (idx as f32) * 22.0 + t * 75.0;
                            let (r, g, b) = pastel_hsv_to_rgb(hue);
                            *color = ColorRgba { r, g, b, a: 1.0 };
                        }
                    }

                    {
                        let Some(spirits) = chunk.component_mut::<CuteSpirit>(4) else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for (row, spirit) in spirits.iter_mut().enumerate() {
                            let idx = global_base + row;
                            let outer_ring = idx.is_multiple_of(2);
                            let angular_speed: f32 = if outer_ring { 0.85 } else { -1.15 };
                            let slot_angle =
                                (idx as f32) * core::f32::consts::FRAC_PI_6 + t * angular_speed;
                            let tangent_x = -slot_angle.sin() * angular_speed.signum();
                            let tangent_z = slot_angle.cos() * angular_speed.signum();
                            *spirit = CuteSpirit {
                                yaw: tangent_x.atan2(tangent_z),
                                ear_wiggle: 0.35 * (t * 6.0 + idx as f32).sin(),
                                blush: 0.65 + 0.35 * (t * 3.0 + idx as f32).cos().abs(),
                            };
                        }
                    }

                    global_base += count;
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: CarouselMod);
