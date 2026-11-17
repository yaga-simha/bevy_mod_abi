//! Mod A: Shield Emitter Mod (`mod_shield_emitter`).
//!
//! Registers a runtime dynamic component (`"mod_shield_emitter::EnergyShield"`) via
//! [`GuestApp::register_component`], spawns autonomous drone entities carrying that component via
//! batched [`GuestCommands::spawn`], and recharges shields each tick.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// Runtime `#[repr(C)]` component (`12` bytes, `4`-byte aligned) introduced dynamically by Mod A.
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

/// Per-`World` guest plugin for `mod_shield_emitter`.
#[derive(Default)]
pub struct ShieldEmitterMod;

impl GuestPlugin for ShieldEmitterMod {
    const NAME: &'static CStr = c"mod_shield_emitter";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        // 1. Register runtime dynamic component `"mod_shield_emitter::EnergyShield"`.
        let shield_h = match app.register_component::<EnergyShield>() {
            Ok(h) => h,
            Err(err) => return err,
        };

        // 2. Spawner system: spawns 3 drones on the first tick using per-World closure state.
        let mut spawned = false;
        let s1_res = app
            .system(c"shield_spawner_system")
            .schedule(ScheduleId::UPDATE)
            .build(move |ctx| {
                if spawned {
                    return FfiResult::OK;
                }
                spawned = true;

                let queue = ctx.commands();
                let drone_specs = [
                    EnergyShield {
                        current: 100.0,
                        max: 100.0,
                        frequency_mhz: 142,
                    },
                    EnergyShield {
                        current: 80.0,
                        max: 100.0,
                        frequency_mhz: 242,
                    },
                    EnergyShield {
                        current: 60.0,
                        max: 100.0,
                        frequency_mhz: 342,
                    },
                ];

                for (i, spec) in drone_specs.iter().enumerate() {
                    let entity = match queue.spawn(&[SpawnEntry::new(shield_h, spec)]) {
                        Ok(e) => e,
                        Err(err) => return err,
                    };
                    println!(
                        "  [Mod A::spawner] Spawned Drone #{} ({:?}) with EnergyShield({:.1}/{:.1} MW, {} MHz)",
                        i + 1,
                        entity,
                        spec.current,
                        spec.max,
                        spec.frequency_mhz
                    );
                }

                FfiResult::OK
            });
        if !s1_res.is_ok() {
            return s1_res;
        }

        // 3. Recharge system: recharges shields (+5.0 MW) and increments harmonic frequency.
        app.system(c"shield_recharge_system")
            .query(|q| q.write_handle(shield_h))
            .schedule(ScheduleId::UPDATE)
            .after(c"shield_spawner_system")
            .build(|ctx| {
                while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                    let Some(shields) = chunk.component_mut::<EnergyShield>(0) else {
                        return FfiResult::TYPE_MISMATCH;
                    };

                    for shield in shields {
                        let before = shield.current;
                        shield.current = (shield.current + 5.0).min(shield.max);
                        shield.frequency_mhz = shield.frequency_mhz.wrapping_add(1);
                        let delta = shield.current - before;

                        if delta > 0.0 {
                            println!(
                                "  [Mod A::recharge] Recharged drone shield: {:.1} -> {:.1} MW (+{:.1} MW)",
                                before, shield.current, delta
                            );
                        }
                    }
                }
                FfiResult::OK
            })
    }
}

export_mod!(plugin: ShieldEmitterMod);
