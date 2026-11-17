//! Example 2: Multi-System Pipeline Guest Mod (`example_02_guest_mod`).
//!
//! Demonstrates three ordered systems built with [`GuestApp::system`] and typed tuple queries
//! ([`SystemBuilder::query_typed`], [`SystemContext::single_query`], and
//! [`SystemContext::single_query_and_commands`]) without magic column index literals (`G-03`),
//! query inclusion/exclusion filters ([`With`] / [`Without`]), pre-resolved [`BamComponent`]
//! handles with single-archetype batched [`GuestCommands::spawn`], and per-`World` instance state
//! cleanup on unload.

use bevy_abi_mod_guest::prelude::*;
use core::ffi::CStr;

/// Shared 3D position component (`"example::Position"`).
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

/// Shared 3D velocity component (`"example::Velocity"`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

impl GuestComponent for Velocity {
    const NAME: &'static CStr = c"example::Velocity";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Shared hit-point component (`"example::Health"`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Health")]
pub struct Health {
    /// Current hit points.
    pub current: f32,
    /// Maximum hit points.
    pub max: f32,
}

impl GuestComponent for Health {
    const NAME: &'static CStr = c"example::Health";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Player ship tag (`"example::PlayerTag"`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::PlayerTag")]
pub struct PlayerTag {
    /// Player identifier.
    pub id: u32,
}

impl GuestComponent for PlayerTag {
    const NAME: &'static CStr = c"example::PlayerTag";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

/// Autonomous drone tag (`"example::DroneTag"`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::DroneTag")]
pub struct DroneTag {
    /// Squadron identifier.
    pub squad: u32,
}

impl GuestComponent for DroneTag {
    const NAME: &'static CStr = c"example::DroneTag";
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[derive(Default)]
struct PipelineState {
    tick_counter: u32,
    reinforcements_spawned: u32,
}

/// Per-`World` guest plugin registering the three-stage fleet simulation pipeline.
#[derive(Default)]
pub struct MultiSystemPipelineMod;

impl GuestPlugin for MultiSystemPipelineMod {
    const NAME: &'static CStr = c"example_02_guest_mod";
    const VERSION: &'static CStr = c"0.2.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        // Pre-resolve handles once during init for O(1) batched spawns on the hot path (`C-05`).
        let pos_h = match app.resolve_component::<Position>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let vel_h = match app.resolve_component::<Velocity>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let health_h = match app.resolve_component::<Health>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let drone_tag_h = match app.resolve_component::<DroneTag>() {
            Ok(h) => h,
            Err(err) => return err,
        };

        // 1. KinematicsMovementSystem: typed tuple query `(&mut Position, &Velocity)` (`G-03`).
        let res1 = app
            .system(c"KinematicsMovementSystem")
            .query_typed::<(&mut Position, &Velocity)>()
            .schedule(ScheduleId::UPDATE)
            .build(|ctx| {
                ctx.single_query::<(&mut Position, &Velocity)>()
                    .for_each(|(pos, vel)| {
                        for (p, v) in pos.iter_mut().zip(vel.iter()) {
                            p.x += v.x * 0.1;
                            p.y += v.y * 0.1;
                        }
                    })
            });
        if !res1.is_ok() {
            return res1;
        }

        // 2. FleetLifecycleSystem: typed tuple query `(FfiEntity, &mut Health, &Position, Without<PlayerTag>)`.
        //    Captures `PipelineState` inside this world's GuestInstance — freed on `shutdown`.
        let mut state = PipelineState::default();
        let res2 = app
            .system(c"FleetLifecycleSystem")
            .query_typed::<(FfiEntity, &mut Health, &Position, Without<PlayerTag>)>()
            .schedule(ScheduleId::UPDATE)
            .after(c"KinematicsMovementSystem")
            .build(move |ctx| {
                state.tick_counter += 1;
                let (mut query, queue) = ctx.single_query_and_commands::<(
                    FfiEntity,
                    &mut Health,
                    &Position,
                    Without<PlayerTag>,
                )>();

                let iter_status = query.for_each(|(entities, healths, _positions, ())| {
                    for (&entity, health) in entities.iter().zip(healths.iter_mut()) {
                        let new_health = health.current - 10.0;
                        if new_health <= 0.0 {
                            let status = queue.despawn(entity);
                            if !status.is_ok() {
                                return status;
                            }
                        } else {
                            health.current = new_health;
                        }
                    }
                    FfiResult::OK
                });
                if !iter_status.is_ok() {
                    return iter_status;
                }

                // Spawn reinforcement Drone Gamma on tick 1 in a single batched archetype move.
                if state.tick_counter == 1 && state.reinforcements_spawned == 0 {
                    let pos = Position {
                        x: 30.0,
                        y: 0.0,
                        z: 0.0,
                    };
                    let vel = Velocity {
                        x: 2.0,
                        y: 1.0,
                        z: 0.0,
                    };
                    let hp = Health {
                        current: 50.0,
                        max: 50.0,
                    };
                    let tag = DroneTag { squad: 2 };
                    if let Err(err) = queue.spawn(&[
                        SpawnEntry::new(pos_h, &pos),
                        SpawnEntry::new(vel_h, &vel),
                        SpawnEntry::new(health_h, &hp),
                        SpawnEntry::new(drone_tag_h, &tag),
                    ]) {
                        return err;
                    }
                    state.reinforcements_spawned += 1;
                }

                FfiResult::OK
            });
        if !res2.is_ok() {
            return res2;
        }

        // 3. PlayerRegenSystem: typed tuple query `(&mut Health, With<PlayerTag>)`.
        app.system(c"PlayerRegenSystem")
            .query_typed::<(&mut Health, With<PlayerTag>)>()
            .schedule(ScheduleId::UPDATE)
            .after(c"FleetLifecycleSystem")
            .run_if_fn(|| true)
            .build(|ctx| {
                ctx.single_query::<(&mut Health, With<PlayerTag>)>()
                    .for_each(|(healths, ())| {
                        for health in healths {
                            health.current = (health.current + 5.0).min(health.max);
                        }
                    })
            })
    }
}

export_mod!(plugin: MultiSystemPipelineMod);
