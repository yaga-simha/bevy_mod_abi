#![forbid(unsafe_code)]

//! Example 1: Minimal ABI `0.3.0` Guest Mod (`minimal_guest_mod`).
//!
//! Exports `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` via
//! [`export_mod!`] with per-`World` instance state, derives [`AbiPod`] and [`GuestComponent`]
//! with zero boilerplate, and registers a typed `Update` system that increments every
//! [`Position`] column slice in place without Bevy dependencies.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// Guest mirror of the host's `"example::Position"` component (`GuestComponent::UUID` matches
/// the host's field-schema hash automatically).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod, GuestComponent)]
#[guest_component(name = "example::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

/// Per-`World` guest plugin instance for `minimal_guest_mod`.
#[derive(Default)]
pub struct MinimalHandshakeMod;

impl GuestPlugin for MinimalHandshakeMod {
    const NAME: &'static CStr = c"minimal_guest_mod";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"position_nudge")
            .query_typed::<&mut Position>()
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                ctx.single_query::<&mut Position>().for_each(|positions| {
                    for pos in positions {
                        pos.x += 1.0;
                        pos.y += 2.0;
                        pos.z += 3.0;
                    }
                })
            })
    }
}

export_mod!(plugin: MinimalHandshakeMod);
