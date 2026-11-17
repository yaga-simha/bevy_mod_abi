//! Example 1: Minimal ABI `0.2.0` Guest Mod (`minimal_guest_mod`).
//!
//! Exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` via
//! [`export_mod!`] with per-`World` instance state, and registers a single `Update` system that
//! increments every [`Position`] column slice in place without Bevy dependencies.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// Guest mirror of the host's `"example::Position"` component (`ModComponent::UUID` matches
/// the host's field-schema hash).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

impl GuestComponent for Position {
    const NAME: &'static CStr = c"example::Position";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Per-`World` guest plugin instance for `minimal_guest_mod`.
#[derive(Default)]
pub struct MinimalHandshakeMod;

impl GuestPlugin for MinimalHandshakeMod {
    const NAME: &'static CStr = c"minimal_guest_mod";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"position_nudge")
            .query(|q| q.write::<Position>())
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(positions) = chunk.component_mut::<Position>(0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };
                    for pos in positions {
                        pos.x += 1.0;
                        pos.y += 2.0;
                        pos.z += 3.0;
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: MinimalHandshakeMod);
