//! Mod B: Overcharge Siphon Mod (`mod_overcharge_siphon`)
//!
//! Demonstrates:
//! 1. Inter-mod dynamic component query: Mod B queries `EnergyShield` by name.
//! 2. Zero component introduction: Mod B does NOT register `EnergyShield` and did NOT spawn the entities.
//! 3. Hardware-speed access: Iterates archetype chunks via `GuestApp` & `Chunk::column_mut`,
//!    siphons energy (-12.0 MW), and mutates data in-place with zero `unsafe` in system logic.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use guest_sdk::prelude::*;

/// C-ABI layout mirror of `EnergyShield` introduced by Mod A.
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct EnergyShield {
    pub current: f32,
    pub max: f32,
    pub frequency_mhz: u32,
}

// SAFETY: `EnergyShield` is `#[repr(C)]` POD (`f32, f32, u32`) matching Mod A's registered layout.
unsafe impl GuestComponent for EnergyShield {
    const NAME: &'static core::ffi::CStr = c"EnergyShield";
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    // Notice: Mod B does NOT call `register_guest_component`!
    // It directly registers a system querying `"EnergyShield"`, which was introduced by Mod A.
    let queries = [EnergyShield::write_desc()];
    app.add_system(c"shield_siphon_system", &queries, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            let Some(shields) = chunk.column_mut::<EnergyShield>(0) else {
                return FfiResult::INTERNAL_ERROR;
            };

            for (row, shield) in shields.iter_mut().enumerate() {
                let before = shield.current;
                let drain = 12.0f32.min(shield.current);
                shield.current -= drain;

                println!(
                    "  [Mod B::siphon]   Foreign Drone row #{} at {} MHz: Siphoned {:.1} MW ({:.1} -> {:.1} MW remaining)",
                    row + 1,
                    shield.frequency_mhz,
                    drain,
                    before,
                    shield.current
                );
            }
        }

        FfiResult::OK
    })
}

export_mod!(setup);
