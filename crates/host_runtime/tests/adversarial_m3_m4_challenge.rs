#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use bevy_app::App;
use common::*;
use host_runtime::{AbiModPlugin, ModAppExt};
use std::collections::HashMap;

/// Test 1: Extreme multi-archetype scaling (32 distinct archetypes, 13,000+ entities,
/// empty table edge cases, single-entity table edge cases, prime entity counts,
/// mid-stream dynamic archetype additions, and mathematical arithmetic oracle).
#[test]
fn test_extreme_multi_archetype_scaling_and_arithmetic_oracle() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<CombatHealth>();
    app.register_mod_component::<NameTag>();
    app.register_mod_component::<MetadataTag>();
    app.register_mod_component::<Mass>();
    app.register_mod_component::<Armor>();

    // We will construct 32 distinct archetypes using a 5-bit mask for supplementary components:
    // Bit 0: Velocity
    // Bit 1: CombatHealth
    // Bit 2: NameTag
    // Bit 3: Mass
    // Bit 4: Armor
    let mut expected_arithmetic = HashMap::new();
    let mut entity_archetype_mask = HashMap::new();
    let mut total_spawned = 0;

    for mask in 0u32..32 {
        let count = match mask {
            0 => 0,   // Empty table edge case
            1 => 1,   // Single-entity table edge case
            2 => 7,   // Prime count edge case
            3 => 13,  // Prime count edge case
            4 => 64,  // Power-of-two alignment
            _ => 500, // Large chunk
        };

        for i in 0..count {
            let id = (mask * 10_000 + i) as usize;
            let init_pos = Position {
                x: id as f32,
                y: 0.0,
                z: 0.0,
            };

            let entity_id = match mask {
                0 => unreachable!(),
                1 => app
                    .world_mut()
                    .spawn((
                        init_pos,
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                    ))
                    .id(),
                2 => app
                    .world_mut()
                    .spawn((
                        init_pos,
                        CombatHealth {
                            current: 1000,
                            max: 1000,
                        },
                    ))
                    .id(),
                3 => app
                    .world_mut()
                    .spawn((
                        init_pos,
                        NameTag {
                            label: format!("tag_{id}"),
                        },
                    ))
                    .id(),
                4 => app.world_mut().spawn((init_pos, Mass { value: 10.0 })).id(),
                5 => app
                    .world_mut()
                    .spawn((
                        init_pos,
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        Armor { defense: 50 },
                    ))
                    .id(),
                6 => app
                    .world_mut()
                    .spawn((
                        init_pos,
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        CombatHealth {
                            current: 1000,
                            max: 1000,
                        },
                        NameTag {
                            label: format!("hero_{id}"),
                        },
                    ))
                    .id(),
                7 => app
                    .world_mut()
                    .spawn((init_pos, Mass { value: 20.0 }, Armor { defense: 100 }))
                    .id(),
                _ => {
                    let has_vel = (mask & 1) != 0;
                    let has_health = (mask & 2) != 0;
                    let has_tag = (mask & 4) != 0;
                    let has_mass = (mask & 8) != 0;
                    let has_armor = (mask & 16) != 0;

                    let mut entity_cmds = app.world_mut().spawn(init_pos);
                    if has_vel {
                        entity_cmds.insert(Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        });
                    }
                    if has_health {
                        entity_cmds.insert(CombatHealth {
                            current: 1000,
                            max: 1000,
                        });
                    }
                    if has_tag {
                        entity_cmds.insert(NameTag {
                            label: format!("ent_{id}"),
                        });
                    }
                    if has_mass {
                        entity_cmds.insert(Mass { value: 5.0 });
                    }
                    if has_armor {
                        entity_cmds.insert(Armor { defense: 10 });
                    }
                    entity_cmds.id()
                }
            };

            entity_archetype_mask.insert(entity_id, mask);
            expected_arithmetic.insert(entity_id, init_pos);
            total_spawned += 1;
        }
    }

    assert!(
        total_spawned > 13_000,
        "Spawned entity count must be > 13,000 across 32 archetypes"
    );

    let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
    let c_id = app.load_mod_tracked(&c_dylib).expect("load c mod");

    const TICKS: usize = 10;
    for tick in 1..=TICKS {
        app.update();

        // On tick 5: edge condition - mid-stream archetype mutation (dynamic entity additions)
        if tick == 5 {
            for j in 0..200 {
                let id = 999_000 + j;
                let init_pos = Position {
                    x: id as f32,
                    y: 0.0,
                    z: 0.0,
                };
                let e = app
                    .world_mut()
                    .spawn((
                        init_pos,
                        Velocity {
                            x: 1.0,
                            y: 2.0,
                            z: 0.0,
                        },
                        Mass { value: 99.0 },
                        Armor { defense: 99 },
                        MetadataTag {
                            label: format!("mid_stream_{j}"),
                        },
                    ))
                    .id();

                entity_archetype_mask.insert(e, 0xFF);
                let mut expected_after_5 = init_pos;
                expected_after_5.x += 5.0 * 1.0;
                expected_after_5.y += 5.0 * 2.0;
                expected_after_5.z += 5.0 * 1.0;
                expected_arithmetic.insert(e, expected_after_5);
            }
        }
    }

    for (&entity, &mask) in &entity_archetype_mask {
        if mask == 0xFF {
            continue;
        }

        let init = expected_arithmetic[&entity];
        let has_vel = match mask {
            1 | 5 | 6 => true,
            2 | 3 | 4 | 7 => false,
            _ => (mask & 1) != 0,
        };

        let mut expected = init;
        if has_vel {
            expected.x += (TICKS as f32) * 1.0;
            expected.y += (TICKS as f32) * 2.0;
        }
        expected.z += (TICKS as f32) * 1.0;

        expected_arithmetic.insert(entity, expected);
    }

    let world = app.world();
    let mut verified_count = 0;

    for (&entity, &expected) in &expected_arithmetic {
        let actual = world
            .get::<Position>(entity)
            .expect("Entity must have Position");
        assert_eq!(
            actual.x, expected.x,
            "Entity {:?} (mask {}) x-coord mismatch",
            entity, entity_archetype_mask[&entity]
        );
        assert_eq!(
            actual.y, expected.y,
            "Entity {:?} (mask {}) y-coord mismatch",
            entity, entity_archetype_mask[&entity]
        );
        assert_eq!(
            actual.z, expected.z,
            "Entity {:?} (mask {}) z-coord mismatch",
            entity, entity_archetype_mask[&entity]
        );
        verified_count += 1;
    }

    assert_eq!(
        verified_count,
        expected_arithmetic.len(),
        "All entities must be verified against arithmetic oracle"
    );

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();
}

/// Test 2: Scale stress test across 100,000 entities in 16 fragmented archetypes.
/// Tests chunk iteration traversal across 100,000 entities with disjoint query matching:
/// 50,000 entities match (Position + Velocity) while 50,000 entities match (Position + Mass without Velocity).
/// Verifies that chunk iteration processes all 100k entities without memory corruption or cross-archetype bleed.
#[test]
fn test_100k_scale_fragmented_archetype_traversal_and_arithmetic() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    app.register_mod_component::<Position>();
    app.register_mod_component::<Velocity>();
    app.register_mod_component::<Mass>();
    app.register_mod_component::<Armor>();

    const SCALE: usize = 100_000;
    const ARCHETYPES: usize = 16;
    const PER_ARCH: usize = SCALE / ARCHETYPES;

    // Spawn 100,000 entities across 16 archetypes:
    // Even archetypes (0, 2, 4, ... 14): Position + Velocity + Armor(i) -> 50,000 entities
    // Odd archetypes (1, 3, 5, ... 15): Position + Mass(i) (NO Velocity) -> 50,000 entities
    for arch in 0..ARCHETYPES {
        let is_kinematic = arch % 2 == 0;
        for i in 0..PER_ARCH {
            let id = arch * PER_ARCH + i;
            let p = Position {
                x: id as f32,
                y: 0.0,
                z: 0.0,
            };
            if is_kinematic {
                app.world_mut().spawn((
                    p,
                    Velocity {
                        x: 1.0,
                        y: 3.0,
                        z: 0.0,
                    },
                    Armor {
                        defense: arch as i32,
                    },
                ));
            } else {
                app.world_mut().spawn((p, Mass { value: arch as f32 }));
            }
        }
    }

    let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
    let c_id = app.load_mod_tracked(&c_dylib).unwrap();

    // Run 5 ticks
    for _ in 0..5 {
        app.update();
    }

    // Verify 50,000 kinematic entities:
    // Rust mod: pos.x += 5 * 1.0 = 5.0, pos.y += 5 * 3.0 = 15.0
    // C mod: pos.z += 5 * 1.0 = 5.0 (since vz == 0.0)
    let mut kin_count = 0;
    let mut non_kin_count = 0;

    let mut query = app
        .world_mut()
        .query::<(&Position, Option<&Velocity>, Option<&Mass>)>();
    for (pos, vel, mass) in query.iter(app.world()) {
        if vel.is_some() {
            assert_eq!(pos.y, 15.0, "Kinematic entity pos.y must be 15.0");
            assert_eq!(pos.z, 5.0, "Kinematic entity pos.z must be 5.0");
            kin_count += 1;
        } else if mass.is_some() {
            assert_eq!(pos.y, 0.0, "Non-kinematic entity pos.y must remain 0.0");
            assert_eq!(
                pos.z, 5.0,
                "Non-kinematic entity pos.z must be 5.0 from C mod"
            );
            non_kin_count += 1;
        }
    }

    assert_eq!(
        kin_count, 50_000,
        "Must have exactly 50,000 kinematic entities"
    );
    assert_eq!(
        non_kin_count, 50_000,
        "Must have exactly 50,000 non-kinematic entities"
    );

    app.unload_mod(r_id).unwrap();
    app.unload_mod(c_id).unwrap();
}
