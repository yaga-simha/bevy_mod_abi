#![forbid(unsafe_code)]

//! Mod 3: Gravitational Vortex and Cyclone Swirl (`mod_vortex`, `ABI 0.2.0`).
//!
//! Exerts centripetal pull toward the arena center and tangential spiral acceleration.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;
const CENTER_X: f32 = ARENA_WIDTH / 2.0;
const CENTER_Y: f32 = ARENA_HEIGHT / 2.0;
const FLOOR_Y: f32 = 1.0;
const CEIL_Y: f32 = ARENA_HEIGHT - 1.0;
const MIN_X: f32 = 1.0;
const MAX_X: f32 = ARENA_WIDTH - 1.0;
const DT: f32 = 1.0 / 60.0;

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

/// Guest mirror of `example::Velocity`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, ModComponent)]
#[mod_component(name = "example::Velocity")]
pub struct Velocity {
    /// Horizontal velocity.
    pub x: f32,
    /// Vertical velocity.
    pub y: f32,
    /// Depth velocity.
    pub z: f32,
}

impl GuestComponent for Velocity {
    const NAME: &'static CStr = c"example::Velocity";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Per-World guest plugin for `mod_vortex`.
#[derive(Default)]
pub struct VortexMod;

impl GuestPlugin for VortexMod {
    const NAME: &'static CStr = c"mod_vortex";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"VortexSwirlSystem")
            .query(|q| q.write::<Position>().write::<Velocity>())
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some((positions, velocities)) =
                        chunk.component_write_2::<Position, Velocity>(0, 1)
                    else {
                        return FfiResult::TYPE_MISMATCH;
                    };

                    for (pos, vel) in positions.iter_mut().zip(velocities.iter_mut()) {
                        let dx = CENTER_X - pos.x;
                        let dy = CENTER_Y - pos.y;
                        let r = (dx * dx + dy * dy).sqrt();
                        let r_clamped = r.max(1.5);

                        let ur_x = dx / r_clamped;
                        let ur_y = dy / r_clamped;
                        let ut_x = -dy / r_clamped;
                        let ut_y = dx / r_clamped;

                        let a_r = if r < 5.0 {
                            -25.0
                        } else {
                            (350.0 / (r_clamped + 8.0)).min(60.0)
                        };
                        let a_t = (450.0 / (r_clamped.sqrt() + 2.0)).min(70.0);

                        let ax = a_r * ur_x + a_t * ut_x;
                        let ay = a_r * ur_y + a_t * ut_y;

                        vel.x = (vel.x + ax * DT) * 0.985;
                        vel.y = (vel.y + ay * DT) * 0.985;

                        if pos.x <= MIN_X {
                            pos.x = MIN_X;
                            vel.x = -vel.x * 0.7;
                        } else if pos.x >= MAX_X {
                            pos.x = MAX_X;
                            vel.x = -vel.x * 0.7;
                        }

                        if pos.y <= FLOOR_Y {
                            pos.y = FLOOR_Y;
                            vel.y = -vel.y * 0.7;
                        } else if pos.y >= CEIL_Y {
                            pos.y = CEIL_Y;
                            vel.y = -vel.y * 0.7;
                        }
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: VortexMod);
