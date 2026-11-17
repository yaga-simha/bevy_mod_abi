#![forbid(unsafe_code)]

//! Mod 1: Jelly Boing Hop (`mod_boing`, `ABI 0.2.0`).
//!
//! Applies 3D gravity and rhythmic ground hops, squash-and-stretch `Scale3D` deformation,
//! floppy ear motion, and landing blush using per-World closure state (`L-02`).

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

const MEADOW_RADIUS: f32 = 8.2;
const GRAVITY: f32 = 16.0;
const DT: f32 = 1.0 / 60.0;
const BATCH: usize = 64;

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

#[derive(Clone, Copy, Default)]
struct BoingDerived {
    landed: bool,
    scale: Scale3D,
    yaw_override: Option<f32>,
    ear_wiggle: f32,
}

/// Per-World guest plugin for `mod_boing`.
#[derive(Default)]
pub struct BoingMod;

impl GuestPlugin for BoingMod {
    const NAME: &'static CStr = c"mod_boing";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        let mut tick: u64 = 0;
        app.system(c"JellyBoingSystem")
            .query(|q| {
                q.write::<Position>()
                    .write::<Velocity>()
                    .write::<Scale3D>()
                    .write::<CuteSpirit>()
            })
            .build(move |ctx| {
                let current_tick = tick;
                tick = tick.wrapping_add(1);

                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let n = chunk.len();
                    let mut start = 0_usize;
                    let mut derived = [BoingDerived::default(); BATCH];

                    while start < n {
                        let batch_len = (n - start).min(BATCH);

                        {
                            let Some((positions, velocities)) =
                                chunk.component_write_2::<Position, Velocity>(0, 1)
                            else {
                                return FfiResult::TYPE_MISMATCH;
                            };

                            for (k, out) in derived[..batch_len].iter_mut().enumerate() {
                                let row = start + k;
                                let pos = &mut positions[row];
                                let vel = &mut velocities[row];

                                vel.y -= GRAVITY * DT;
                                pos.y += vel.y * DT;

                                let phase = (row as f32) * 1.618 + (current_tick as f32) * 0.07;
                                let landed = pos.y <= 0.0;
                                if landed {
                                    pos.y = 0.0;
                                    vel.y = 5.4 + 1.8 * phase.sin().abs();

                                    let dist_sq = pos.x * pos.x + pos.z * pos.z;
                                    if dist_sq > (MEADOW_RADIUS * 0.65) * (MEADOW_RADIUS * 0.65) {
                                        let dist = dist_sq.sqrt().max(0.01);
                                        vel.x = (-pos.x / dist) * 2.4 + phase.cos() * 0.8;
                                        vel.z = (-pos.z / dist) * 2.4 + phase.sin() * 0.8;
                                    } else {
                                        vel.x = vel.x * 0.85 + phase.cos() * 0.65;
                                        vel.z = vel.z * 0.85 + phase.sin() * 0.65;
                                    }
                                }

                                let r = (pos.x * pos.x + pos.z * pos.z).sqrt();
                                if r > MEADOW_RADIUS {
                                    let nx = pos.x / r;
                                    let nz = pos.z / r;
                                    pos.x = nx * MEADOW_RADIUS;
                                    pos.z = nz * MEADOW_RADIUS;
                                    vel.x = -vel.x * 0.8;
                                    vel.z = -vel.z * 0.8;
                                }

                                let stretch = if pos.y < 0.18 {
                                    0.68
                                } else {
                                    (1.0 + vel.y * 0.055).clamp(0.76, 1.38)
                                };
                                let inv_sqrt = 1.0 / stretch.sqrt();
                                let speed_xz = (vel.x * vel.x + vel.z * vel.z).sqrt();

                                *out = BoingDerived {
                                    landed,
                                    scale: Scale3D {
                                        x: inv_sqrt,
                                        y: stretch,
                                        z: inv_sqrt,
                                    },
                                    yaw_override: (speed_xz > 0.05).then(|| vel.x.atan2(vel.z)),
                                    ear_wiggle: (-vel.y * 0.09).clamp(-0.55, 0.55)
                                        + 0.12 * (phase * 2.0).sin(),
                                };
                            }
                        }

                        {
                            let Some((scales, spirits)) =
                                chunk.component_write_2::<Scale3D, CuteSpirit>(2, 3)
                            else {
                                return FfiResult::TYPE_MISMATCH;
                            };

                            for (k, &d) in derived[..batch_len].iter().enumerate() {
                                let row = start + k;
                                scales[row] = d.scale;
                                let sp = &mut spirits[row];
                                if let Some(yaw) = d.yaw_override {
                                    sp.yaw = yaw;
                                }
                                sp.ear_wiggle = d.ear_wiggle;
                                sp.blush = if d.landed {
                                    1.0
                                } else {
                                    (sp.blush - DT * 1.4).max(0.32)
                                };
                            }
                        }

                        start += batch_len;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: BoingMod);
