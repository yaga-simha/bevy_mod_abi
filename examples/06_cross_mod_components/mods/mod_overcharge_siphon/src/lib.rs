//! Mod B: Overcharge Siphon Mod (`mod_overcharge_siphon`).
//!
//! Resolves and mutates `"mod_shield_emitter::EnergyShield"` — a dynamic component introduced at
//! runtime by Mod A — without registering the component or spawning the entities itself, ordered
//! cross-mod after `"mod_shield_emitter::shield_recharge_system"`.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// C-ABI layout mirror of `"mod_shield_emitter::EnergyShield"` introduced by Mod A.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
pub struct EnergyShield {
    /// Current shield charge (MW).
    pub current: f32,
    /// Maximum shield capacity (MW).
    pub max: f32,
    /// Harmonic modulation frequency (MHz).
    pub frequency_mhz: u32,
}

impl GuestComponent for EnergyShield {
    const NAME: &'static CStr = c"mod_shield_emitter::EnergyShield";
}

/// Per-`World` guest plugin for `mod_overcharge_siphon`.
#[derive(Default)]
pub struct OverchargeSiphonMod;

impl GuestPlugin for OverchargeSiphonMod {
    const NAME: &'static CStr = c"mod_overcharge_siphon";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        app.system(c"shield_siphon_system")
            .query(|q| q.write::<EnergyShield>())
            .schedule(ScheduleId::UPDATE)
            .after(c"mod_shield_emitter::shield_recharge_system")
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(shields) = chunk.component_mut::<EnergyShield>(0) else {
                        return FfiResult::TYPE_MISMATCH;
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
}

export_mod!(plugin: OverchargeSiphonMod);
