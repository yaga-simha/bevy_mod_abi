//! Mod A: Shield Emitter Mod (`mod_shield_emitter`)
//!
//! Demonstrates:
//! 1. Introducing a brand-new component type (`EnergyShield`) at runtime via `GuestApp::register_guest_component`.
//!    Neither the host game engine nor any other mod knows about this component at host compile-time.
//! 2. Spawning autonomous entities and attaching `EnergyShield` dynamically using deferred commands
//!    (`SystemContext::commands`) with per-instance closure state (zero global statics, zero memory leaks).
//! 3. Running an active recharge system that modifies `EnergyShield` each tick in 100% safe Rust.

#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use guest_sdk::prelude::*;

/// Runtime structured data introduced dynamically by Mod A.
/// Layout: 12 bytes, 4-byte aligned (POD).
#[derive(Clone, Copy, Debug, PartialEq)]
#[repr(C)]
pub struct EnergyShield {
    /// Current shield charge (MW).
    pub current: f32,
    /// Maximum shield capacity (MW).
    pub max: f32,
    /// Harmonic modulation frequency (MHz).
    pub frequency_mhz: u32,
}

// SAFETY: `EnergyShield` is `#[repr(C)]` POD (`f32, f32, u32`) with no invalid bit patterns or heap pointers.
unsafe impl GuestComponent for EnergyShield {
    const NAME: &'static core::ffi::CStr = c"EnergyShield";
}

fn setup(app: &mut GuestApp<'_>) -> FfiResult {
    // 1. Introduce & register runtime component: `EnergyShield`
    let reg_res = app.register_guest_component::<EnergyShield>();
    if reg_res.is_err() {
        return reg_res;
    }

    // 2. Register System 1: Autonomous Shield Drone Spawner (runs once on first tick;
    //    `spawned` lives inside the guest-managed closure and is freed automatically on unload!)
    let mut spawned = false;
    let s1_res = app.add_system(c"shield_spawner_system", &[], move |ctx| {
        if spawned {
            return FfiResult::OK;
        }
        spawned = true;

        let queue = ctx.commands();
        if queue.is_null() {
            return FfiResult::NULL_POINTER;
        }

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
            let entity = queue.spawn_empty();
            if entity.0 == 0 {
                return FfiResult::INTERNAL_ERROR;
            }

            let res = queue.insert_component(entity, spec);
            if res.is_err() {
                return res;
            }

            println!(
                "  [Mod A::spawner] Spawned Drone #{} (FfiEntity {:?}) with EnergyShield({:.1}/{:.1} MW, {} MHz)",
                i + 1,
                entity,
                spec.current,
                spec.max,
                spec.frequency_mhz
            );
        }

        FfiResult::OK
    });
    if s1_res.is_err() {
        return s1_res;
    }

    // 3. Register System 2: Shield Generator & Recharge Loop
    let recharge_queries = [EnergyShield::write_desc()];
    app.add_system(c"shield_recharge_system", &recharge_queries, |ctx| {
        while let Some(mut chunk) = ctx.iter().next_chunk_view() {
            let Some(shields) = chunk.column_mut::<EnergyShield>(0) else {
                return FfiResult::INTERNAL_ERROR;
            };

            for shield in shields.iter_mut() {
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

export_mod!(setup);
