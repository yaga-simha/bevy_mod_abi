#![forbid(unsafe_code)]

//! Standalone zero-Bevy `#![forbid(unsafe_code)]` Rust C-ABI guest mod for Example 10
//! (`10_bevy20_full_companion`).
//!
//! Compiles as two distinct native guest plugins depending on Cargo feature selection:
//! - Default (`companion_core_mod`, Mod A):
//!   - Registers dynamic POD component `companion_core_mod::OrbitalBeacon` with declarative
//!     `BamFieldDesc` metadata and `SCHEMA_FINGERPRINT` (compatible with `ModReflectPlugin` and
//!     `ModSaveGamePlugin`).
//!   - Installs runtime `cmd_observe` observers on Bevy `0.20` flattened `PointerPress`
//!     (`bevy_picking::events::PointerPress`) and headless UI `FfiValueChange`
//!     (`bevy_abi_mod_ui::FfiValueChange`) that mutate persistent `companion::Beacon` state.
//!   - Animates 3D `FfiPointLight` intensity across the C-ABI boundary.
//! - Feature `mod-b` (`companion_fx_mod`, Mod B):
//!   - Registers dynamic POD component `companion_fx_mod::PlasmaAura`, attaches it to persistent
//!     beacons, ticks an aura pulse each frame, and spawns one ephemeral `ModScoped` entity to
//!     demonstrate Quick-Save exclusion and `StripMissingModComponents` recovery when Mod B is
//!     disabled.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// Guest mirror of the host's persistent `"companion::Beacon"` component.
///
/// Layout: 24 bytes, 4-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "companion::Beacon")]
pub struct CompanionBeacon {
    /// Stable application-assigned beacon identifier (`1`, `2`, `3`, ...).
    pub beacon_id: u32,
    /// Cumulative `PointerPress` press count observed via guest `cmd_observe`.
    pub press_count: u32,
    /// Raw `FfiPointerButton` discriminant from the most recent `PointerPress`.
    pub last_button: u32,
    /// Live energy level incremented by ticks and boosted by pointer presses.
    pub energy: f32,
    /// Most recently selected HUD tab index recorded by the `FfiValueChange` guest observer.
    pub active_tab_index: u32,
    /// Cumulative `FfiValueChange` UI events handled by the guest observer.
    pub ui_events: u32,
}

impl GuestComponent for CompanionBeacon {
    const NAME: &'static CStr = c"companion::Beacon";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[cfg(not(feature = "mod-b"))]
pub use mod_a::{CompanionCoreMod, GuestPointLight, OrbitalBeacon};

#[cfg(not(feature = "mod-b"))]
mod mod_a {
    use super::CompanionBeacon;
    use bevy_abi_mod_guest::prelude::*;
    use core::ffi::CStr;
    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    /// Dynamic POD component owned by Mod A (`companion_core_mod`) attached to persistent beacons.
    ///
    /// Layout: 16 bytes, 4-byte alignment, zero implicit padding.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
    #[mod_component(name = "companion_core_mod::OrbitalBeacon")]
    pub struct OrbitalBeacon {
        /// Orbital phase accumulator in radians.
        pub orbit_phase: f32,
        /// Harmonic gain boosted whenever the beacon is clicked/pressed.
        pub harmonic_gain: f32,
        /// Number of `Update` simulation ticks executed by Mod A on this beacon.
        pub ticks: u32,
        /// Number of picking press boosts applied by the guest `cmd_observe` handler.
        pub pick_boosts: u32,
    }

    impl GuestComponent for OrbitalBeacon {
        const NAME: &'static CStr = c"companion_core_mod::OrbitalBeacon";
        const UUID: ComponentUuid = <Self as ModComponent>::UUID;
    }

    /// Guest mirror of `bevy_abi_mod::camera_light::FfiPointLight` (44 bytes, align 4).
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
    #[mod_component(name = "bevy_abi_mod::camera_light::FfiPointLight")]
    pub struct GuestPointLight {
        pub color: [f32; 4],
        pub intensity: f32,
        pub range: f32,
        pub radius: f32,
        pub shadow_depth_bias: f32,
        pub shadow_normal_bias: f32,
        pub shadow_map_near_z: f32,
        pub shadow_maps_enabled: FfiBool,
        pub contact_shadows_enabled: FfiBool,
        pub affects_lightmapped_mesh_diffuse: FfiBool,
        pub reserved0: u8,
    }

    impl GuestComponent for GuestPointLight {
        const NAME: &'static CStr = c"bevy_abi_mod::camera_light::FfiPointLight";
        const UUID: ComponentUuid = <Self as ModComponent>::UUID;
    }

    /// Zero-padding `AbiPod` mirror of `bevy_picking::events::PointerPress` (`FfiPointerPress`).
    ///
    /// Layout: 96 bytes, 8-byte alignment, zero implicit padding.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
    struct GuestPointerPress {
        target: FfiEntity,
        hit_entity: FfiEntity,
        hit_camera: FfiEntity,
        window: FfiEntity,
        pointer_id: u64,
        pointer_id_hi: u64,
        position: [f32; 2],
        hit_depth: f32,
        hit_position: [f32; 3],
        hit_normal: [f32; 3],
        pointer_kind: u8,
        button: u8,
        count: u8,
        has_hit_position: FfiBool,
        has_hit_normal: FfiBool,
        reserved0: [u8; 7],
    }

    /// Zero-padding `AbiPod` mirror of `bevy_abi_mod_ui::FfiValueChange`.
    ///
    /// Layout: 56 bytes, 8-byte alignment, zero implicit padding.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, AbiPod)]
    struct GuestValueChange {
        source: FfiEntity,
        entity_value: FfiEntity,
        f64_value: f64,
        i64_value: i64,
        color_value: [f32; 4],
        f32_value: f32,
        kind: u8,
        bool_value: FfiBool,
        is_final: FfiBool,
        reserved0: u8,
    }

    const VALUE_CHANGE_KIND_OPTIONAL_ENTITY: u8 = 1;

    #[derive(Default)]
    struct SharedObserverState {
        beacons: HashMap<u64, CompanionBeacon>,
        pending_pick_boosts: HashMap<u64, u32>,
        primary_beacon: Option<FfiEntity>,
    }

    /// Per-World guest plugin instance for Mod A (`companion_core_mod`).
    #[derive(Default)]
    pub struct CompanionCoreMod;

    impl GuestPlugin for CompanionCoreMod {
        const NAME: &'static CStr = c"companion_core_mod";
        const VERSION: &'static CStr = c"0.3.0";

        fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
            let Ok(beacon_handle) = app.resolve_component::<CompanionBeacon>() else {
                return FfiResult::NOT_FOUND;
            };
            let Ok(orbital_handle) = app.register_dynamic_component::<OrbitalBeacon>() else {
                return FfiResult::INTERNAL_ERROR;
            };
            let Ok(pointer_press_handle) =
                app.resolve_component_named(Some(c"bevy_picking::events::PointerPress"), None)
            else {
                return FfiResult::NOT_FOUND;
            };
            let Ok(value_change_handle) =
                app.resolve_component_named(Some(c"bevy_abi_mod_ui::FfiValueChange"), None)
            else {
                return FfiResult::NOT_FOUND;
            };

            let shared_state = Arc::new(Mutex::new(SharedObserverState::default()));

            // System 1: attach `OrbitalBeacon` to newly discovered persistent beacons and install
            // runtime `cmd_observe` handlers for `PointerPress` and `FfiValueChange`.
            let bootstrap_state = Arc::clone(&shared_state);
            let mut observed_beacons: HashSet<u64> = HashSet::new();
            let mut ui_observer_installed = false;

            let rc = app
                .system(c"bootstrap_beacons_and_observers")
                .query(|q| q.read::<CompanionBeacon>().optional_read::<OrbitalBeacon>())
                .schedule(ScheduleId::UPDATE)
                .build(move |ctx| {
                    let mut newly_discovered: Vec<(FfiEntity, CompanionBeacon, bool)> = Vec::new();
                    while let Some(chunk) = ctx.iter().next_chunk_view() {
                        let entities = chunk.entities();
                        let Some(beacons) = chunk.component::<CompanionBeacon>(0) else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        let orbitals = chunk.optional_component::<OrbitalBeacon>(1);
                        for (idx, (&entity, &beacon)) in
                            entities.iter().zip(beacons.iter()).enumerate()
                        {
                            let missing_orbital = orbitals.is_none_or(|slice| idx >= slice.len());
                            newly_discovered.push((entity, beacon, missing_orbital));
                        }
                    }

                    {
                        let Ok(mut st) = bootstrap_state.lock() else {
                            return FfiResult::INTERNAL_ERROR;
                        };
                        for &(entity, beacon, _) in &newly_discovered {
                            st.beacons.insert(entity.to_bits(), beacon);
                            if beacon.beacon_id == 1 || st.primary_beacon.is_none() {
                                st.primary_beacon = Some(entity);
                            }
                        }
                    }

                    let cmds = ctx.commands();

                    for (entity, beacon, missing_orbital) in newly_discovered {
                        if missing_orbital {
                            let initial_orbital = OrbitalBeacon {
                                orbit_phase: 0.25 * (beacon.beacon_id as f32),
                                harmonic_gain: 1.0,
                                ticks: 0,
                                pick_boosts: 0,
                            };
                            let status = cmds.insert(entity, orbital_handle, &initial_orbital);
                            if !status.is_ok() {
                                return status;
                            }
                        }

                        if observed_beacons.insert(entity.to_bits()) {
                            let obs_state = Arc::clone(&bootstrap_state);
                            let res = cmds.observe_handle_runtime::<GuestPointerPress, _>(
                                Some("on_companion_pointer_press"),
                                pointer_press_handle,
                                entity,
                                0,
                                BamComponent::INVALID,
                                move |target, ev, obs_cmds| {
                                    let Ok(mut st) = obs_state.lock() else {
                                        return;
                                    };
                                    let press_inc = u32::from(ev.count.max(1));
                                    let depth_bonus = if ev.hit_depth.is_finite() {
                                        ev.hit_depth.max(0.0)
                                    } else {
                                        0.0
                                    };
                                    let entry = st.beacons.entry(target.to_bits()).or_default();
                                    entry.press_count = entry.press_count.saturating_add(press_inc);
                                    entry.last_button = u32::from(ev.button);
                                    entry.energy += 25.0 + depth_bonus;
                                    let updated = *entry;
                                    *st.pending_pick_boosts.entry(target.to_bits()).or_insert(0) +=
                                        1;
                                    let _ = obs_cmds.insert(target, beacon_handle, &updated);
                                },
                            );
                            if let Err(err) = res {
                                return err;
                            }
                        }
                    }

                    if !ui_observer_installed {
                        let ui_state = Arc::clone(&bootstrap_state);
                        let res = cmds.observe_handle_runtime::<GuestValueChange, _>(
                            Some("on_companion_hud_value_change"),
                            value_change_handle,
                            FfiEntity::NULL,
                            0,
                            BamComponent::INVALID,
                            move |_target, ev, obs_cmds| {
                                let Ok(mut st) = ui_state.lock() else {
                                    return;
                                };
                                let Some(primary) = st.primary_beacon else {
                                    return;
                                };
                                let entry = st.beacons.entry(primary.to_bits()).or_default();
                                entry.ui_events = entry.ui_events.saturating_add(1);
                                if ev.kind == VALUE_CHANGE_KIND_OPTIONAL_ENTITY {
                                    entry.active_tab_index = if ev.entity_value.is_null() {
                                        0
                                    } else {
                                        entry.ui_events
                                    };
                                }
                                let updated = *entry;
                                let _ = obs_cmds.insert(primary, beacon_handle, &updated);
                            },
                        );
                        if let Err(err) = res {
                            return err;
                        }
                        ui_observer_installed = true;
                    }

                    FfiResult::OK
                });
            if !rc.is_ok() {
                return rc;
            }

            // System 2: advance orbital beacon simulation & sync live snapshots for observers.
            let tick_state = Arc::clone(&shared_state);
            let rc = app
                .system(c"tick_orbital_beacons")
                .query(|q| q.write::<CompanionBeacon>().write::<OrbitalBeacon>())
                .after(c"bootstrap_beacons_and_observers")
                .schedule(ScheduleId::UPDATE)
                .build(move |ctx| {
                    let Ok(mut st) = tick_state.lock() else {
                        return FfiResult::INTERNAL_ERROR;
                    };
                    while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                        let entity_bits: Vec<u64> =
                            chunk.entities().iter().map(|e| e.to_bits()).collect();
                        let Some((beacons, orbitals)) =
                            chunk.component_write_2::<CompanionBeacon, OrbitalBeacon>(0, 1)
                        else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for ((bits, beacon), orbital) in entity_bits
                            .into_iter()
                            .zip(beacons.iter_mut())
                            .zip(orbitals.iter_mut())
                        {
                            if let Some(boosts) = st.pending_pick_boosts.remove(&bits) {
                                orbital.pick_boosts = orbital.pick_boosts.saturating_add(boosts);
                            }
                            orbital.ticks = orbital.ticks.saturating_add(1);
                            orbital.orbit_phase += 0.25;
                            orbital.harmonic_gain = 1.0 + 0.1 * (orbital.pick_boosts as f32);
                            beacon.energy += 1.0;
                            st.beacons.insert(bits, *beacon);
                        }
                    }
                    FfiResult::OK
                });
            if !rc.is_ok() {
                return rc;
            }

            // System 3: pulse `FfiPointLight` intensity across the C-ABI boundary so
            // `ModCameraLightBridgePlugin` pushes live guest mutations into Bevy's `PointLight`.
            let mut light_step: u32 = 0;
            app.system(c"pulse_companion_lights")
                .query(|q| q.write::<GuestPointLight>())
                .schedule(ScheduleId::UPDATE)
                .build(move |ctx| {
                    light_step = light_step.saturating_add(1);
                    while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                        let Some(lights) = chunk.component_mut::<GuestPointLight>(0) else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for light in lights {
                            light.intensity = 2_400.0 + 125.0 * (light_step as f32);
                        }
                    }
                    FfiResult::OK
                })
        }
    }

    export_mod!(plugin: CompanionCoreMod);
}

#[cfg(feature = "mod-b")]
pub use mod_b::{CompanionFxMod, PlasmaAura};

#[cfg(feature = "mod-b")]
mod mod_b {
    use super::CompanionBeacon;
    use bevy_abi_mod_guest::prelude::*;
    use core::ffi::CStr;

    /// Dynamic POD component owned by Mod B (`companion_fx_mod`) attached to persistent beacons.
    ///
    /// Stripped cleanly by `ModSaveLoadPolicy::StripMissingModComponents` when Mod B is disabled!
    ///
    /// Layout: 16 bytes, 4-byte alignment, zero implicit padding.
    #[repr(C)]
    #[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
    #[mod_component(name = "companion_fx_mod::PlasmaAura")]
    pub struct PlasmaAura {
        /// Emissive plasma aura intensity.
        pub intensity: f32,
        /// Harmonic resonance frequency in Hz.
        pub frequency_hz: f32,
        /// Number of aura pulse frames applied by Mod B.
        pub pulse_count: u32,
        /// Beacon tier mirror.
        pub tier: u32,
    }

    impl GuestComponent for PlasmaAura {
        const NAME: &'static CStr = c"companion_fx_mod::PlasmaAura";
        const UUID: ComponentUuid = <Self as ModComponent>::UUID;
    }

    /// Per-World guest plugin instance for Mod B (`companion_fx_mod`).
    #[derive(Default)]
    pub struct CompanionFxMod;

    impl GuestPlugin for CompanionFxMod {
        const NAME: &'static CStr = c"companion_fx_mod";
        const VERSION: &'static CStr = c"0.3.0";

        fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
            let Ok(aura_handle) = app.register_dynamic_component::<PlasmaAura>() else {
                return FfiResult::INTERNAL_ERROR;
            };

            let mut spawned_ephemeral = false;
            let rc = app
                .system(c"attach_plasma_auras")
                .query(|q| q.read::<CompanionBeacon>().without::<PlasmaAura>())
                .schedule(ScheduleId::UPDATE)
                .build(move |ctx| {
                    let mut to_attach: Vec<(FfiEntity, PlasmaAura)> = Vec::new();
                    while let Some(chunk) = ctx.iter().next_chunk_view() {
                        let entities = chunk.entities();
                        let Some(beacons) = chunk.component::<CompanionBeacon>(0) else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for (&entity, &beacon) in entities.iter().zip(beacons.iter()) {
                            to_attach.push((
                                entity,
                                PlasmaAura {
                                    intensity: 1.5,
                                    frequency_hz: 440.0 + 55.0 * (beacon.beacon_id as f32),
                                    pulse_count: 0,
                                    tier: beacon.beacon_id,
                                },
                            ));
                        }
                    }

                    let cmds = ctx.commands();

                    for (entity, aura) in to_attach {
                        let status = cmds.insert(entity, aura_handle, &aura);
                        if !status.is_ok() {
                            return status;
                        }
                    }

                    // Spawn one ephemeral ModScoped entity (excluded from persistent save archives
                    // and automatically despawned when Mod B is unloaded).
                    if !spawned_ephemeral {
                        let ephemeral_aura = PlasmaAura {
                            intensity: 99.0,
                            frequency_hz: 880.0,
                            pulse_count: 0,
                            tier: 999,
                        };
                        if let Err(err) =
                            cmds.spawn(&[SpawnEntry::new(aura_handle, &ephemeral_aura)])
                        {
                            return err;
                        }
                        spawned_ephemeral = true;
                    }

                    FfiResult::OK
                });
            if !rc.is_ok() {
                return rc;
            }

            app.system(c"pulse_plasma_auras")
                .query(|q| q.write::<PlasmaAura>())
                .after(c"attach_plasma_auras")
                .schedule(ScheduleId::UPDATE)
                .build(|ctx| {
                    while let Some(mut chunk) = ctx.iter().next_chunk_view() {
                        let Some(auras) = chunk.component_mut::<PlasmaAura>(0) else {
                            return FfiResult::TYPE_MISMATCH;
                        };
                        for aura in auras {
                            aura.pulse_count = aura.pulse_count.saturating_add(1);
                            aura.intensity += 0.125;
                        }
                    }
                    FfiResult::OK
                })
        }
    }

    export_mod!(plugin: CompanionFxMod);
}
