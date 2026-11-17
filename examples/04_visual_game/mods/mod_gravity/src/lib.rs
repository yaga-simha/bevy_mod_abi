#![forbid(unsafe_code)]

//! Mod 1: Gravity and Kinetic Bouncer (`mod_gravity`, `ABI 0.2.0`).
//!
//! Applies downward gravitational acceleration and handles elastic floor and wall
//! collisions with restitution and horizontal friction.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

const ARENA_WIDTH: f32 = 120.0;
const ARENA_HEIGHT: f32 = 60.0;
const FLOOR_Y: f32 = 1.0;
const CEIL_Y: f32 = ARENA_HEIGHT - 1.0;
const MIN_X: f32 = 1.0;
const MAX_X: f32 = ARENA_WIDTH - 1.0;

const GRAVITY: f32 = 40.0;
const RESTITUTION: f32 = 0.85;
const FRICTION: f32 = 0.98;
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

/// Per-World guest plugin for `mod_gravity`.
#[derive(Default)]
pub struct GravityMod;

impl GuestPlugin for GravityMod {
    const NAME: &'static CStr = c"mod_gravity";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"GravityPhysicsSystem")
            .query(|q| q.write::<Position>().write::<Velocity>())
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some((positions, velocities)) =
                        chunk.component_write_2::<Position, Velocity>(0, 1)
                    else {
                        return FfiResult::TYPE_MISMATCH;
                    };

                    for (pos, vel) in positions.iter_mut().zip(velocities.iter_mut()) {
                        vel.y -= GRAVITY * DT;

                        if pos.y <= FLOOR_Y {
                            pos.y = FLOOR_Y;
                            vel.y = -vel.y * RESTITUTION;
                            vel.x *= FRICTION;
                        } else if pos.y >= CEIL_Y {
                            pos.y = CEIL_Y;
                            vel.y = -vel.y * RESTITUTION;
                        }

                        if pos.x <= MIN_X {
                            pos.x = MIN_X;
                            vel.x = -vel.x * RESTITUTION;
                        } else if pos.x >= MAX_X {
                            pos.x = MAX_X;
                            vel.x = -vel.x * RESTITUTION;
                        }
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: GravityMod);
