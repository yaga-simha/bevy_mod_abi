//! Microbenchmark: Chunk Iteration Throughput (`ABI 0.2.0`).
//!
//! Compares Native Bevy ECS, Static `#[derive(ModComponent)]` C-ABI VTable access,
//! and Runtime-Registered Dynamic Components (Rust `.cdylib` and C99 `.dylib`).
//! Standard library `Instant` based, zero external benchmark harness dependencies.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::cast_ptr_alignment, unreachable_pub)]

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_app::{App, Update};
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::system::Query;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Host 3D position component exposed to guest mods via `#[derive(ModComponent)]`.
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

/// Host 3D velocity component exposed to guest mods via `#[derive(ModComponent)]`.
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq)]
struct FragTagA(u32);

#[derive(Component, Debug, Clone, Copy, PartialEq)]
struct FragTagB(u32);

#[derive(Component, Debug, Clone, Copy, PartialEq)]
struct FragTagC(u32);

#[derive(Component, Debug, Clone, Copy, PartialEq)]
struct FragTagD(u32);

/// Native Bevy baseline matching the 8-byte `(f32, u32)` payload of `RustModCustomData` / `CModCustomData`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct NativeCustomData {
    /// Primary floating-point stat.
    pub power: f32,
    /// Secondary integer stat.
    pub shield: u32,
}

/// Host-side raw layout mirror used only to initialize and verify bytes in Bevy's table storage
/// for runtime-registered components (`RustModCustomData` and `CModCustomData`).
#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct RawRuntimePayload {
    primary: f32,
    secondary: u32,
}

fn native_custom_system(mut query: Query<&mut NativeCustomData>) {
    for mut item in &mut query {
        item.power += 10.0;
        item.shield += 1;
    }
}

fn native_kinematics_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in &mut query {
        pos.x += vel.x;
        pos.y += vel.y;
        pos.z += vel.z;
    }
}

fn lookup_dynamic_component_id(reg: &ModRegistry, qualified: &str, short: &str) -> ComponentId {
    if let Some(meta) = reg.get_by_name(qualified) {
        return meta.component_id;
    }
    if let Some(meta) = reg.get_by_name(short) {
        return meta.component_id;
    }
    let suffix = format!("::{short}");
    reg.snapshot()
        .entries()
        .iter()
        .find(|entry| entry.name.ends_with(&suffix))
        .unwrap_or_else(|| panic!("runtime component '{qualified}' / '{short}' not found"))
        .component_id
}

fn get_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = if manifest_dir.ends_with("bevy_abi_mod_host") {
        manifest_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    } else {
        manifest_dir
    };

    let fixture_dir = std::env::var_os("BAM_FIXTURE_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| root_dir.join("target").join("fixtures"));

    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let rust_dylib = fixture_dir.join(format!("{prefix}asymmetric_rust_mod.{ext}"));
    let c_dylib = fixture_dir.join(format!("{prefix}asymmetric_c_mod.{ext}"));

    (rust_dylib, c_dylib)
}

fn measure_app_rounds(
    app: &mut App,
    warmup: usize,
    rounds: usize,
    entity_count: usize,
) -> (f64, f64, f64) {
    for _ in 0..warmup {
        app.update();
    }

    let mut round_durations = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let start = Instant::now();
        app.update();
        round_durations.push(start.elapsed());
    }

    round_durations.sort();
    let median_ms = round_durations[rounds / 2].as_secs_f64() * 1000.0;
    let total_time: Duration = round_durations.iter().sum();
    let avg_secs = total_time.as_secs_f64() / (rounds as f64);
    let entities_per_sec = (entity_count as f64) / avg_secs;
    let ns_per_entity = (total_time.as_nanos() as f64) / ((entity_count * rounds) as f64);

    (median_ms, entities_per_sec, ns_per_entity)
}

fn main() {
    let (rust_dylib, c_dylib) = get_dylib_paths();
    assert!(
        rust_dylib.exists(),
        "Fixture missing at {} — run 'cargo xtask fixtures' before 'cargo bench'",
        rust_dylib.display()
    );
    assert!(
        c_dylib.exists(),
        "Fixture missing at {} — run 'cargo xtask fixtures' before 'cargo bench'",
        c_dylib.display()
    );

    const WARMUP: usize = 5;
    const ROUNDS: usize = 30;
    const TOTAL_TICKS: usize = WARMUP + ROUNDS;

    println!(
        "=========================================================================================="
    );
    println!(
        " MICROBENCHMARK: Runtime-Registered vs. Static ModComponent vs. Native Bevy Throughput"
    );
    println!(
        "=========================================================================================="
    );

    for &entity_count in &[1_000_usize, 10_000, 100_000] {
        println!(
            "\n--- Scale: {:>6} Entities ({ROUNDS} measured frames) ---",
            entity_count
        );

        // 1. Native Bevy Component (8-byte struct: power + shield)
        let (nat_med_ms, nat_tput, nat_ns) = {
            let mut app = App::new();
            let mut e0 = None;
            for i in 0..entity_count {
                let id = app
                    .world_mut()
                    .spawn(NativeCustomData {
                        power: i as f32,
                        shield: 1,
                    })
                    .id();
                if i == 0 {
                    e0 = Some(id);
                }
            }
            app.add_systems(Update, native_custom_system);
            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            let d0 = *app.world().get::<NativeCustomData>(e0.unwrap()).unwrap();
            assert_eq!(d0.shield, 1 + TOTAL_TICKS as u32);
            res
        };

        // 2. Native Bevy Kinematics (Position + Velocity)
        let (nat_kin_med_ms, nat_kin_tput, nat_kin_ns) = {
            let mut app = App::new();
            for i in 0..entity_count {
                app.world_mut().spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 2.0,
                        z: 0.0,
                    },
                ));
            }
            app.add_systems(Update, native_kinematics_system);
            measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count)
        };

        // 3. Static `#[derive(ModComponent)]` C-ABI Access (Position + Velocity)
        let (stat_med_ms, stat_tput, stat_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.register_mod_component::<Position>(
                ComponentOptions::new().with_raw_insert().with_default(),
            )
            .expect("register Position");
            app.register_mod_component::<Velocity>(
                ComponentOptions::new().with_raw_insert().with_default(),
            )
            .expect("register Velocity");
            for i in 0..entity_count {
                app.world_mut().spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 2.0,
                        z: 0.0,
                    },
                ));
            }
            let mod_id = app.load_mod(&rust_dylib).expect("load rust mod");
            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            app.unload_mod(mod_id).expect("unload rust mod");
            res
        };

        // 4. Runtime-Registered Dynamic Component — Rust `.cdylib` (`RustModCustomData`)
        let (rt_rust_med_ms, rt_rust_tput, rt_rust_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let mod_id = app.load_mod(&rust_dylib).expect("load rust mod");
            let comp_id = lookup_dynamic_component_id(
                app.world().resource::<ModRegistry>(),
                "asymmetric_rust_mod::RustModCustomData",
                "RustModCustomData",
            );

            let mut e0 = None;
            for i in 0..entity_count {
                let id = app.world_mut().spawn_empty().id();
                let init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let mut emut = app.world_mut().entity_mut(id);
                OwningPtr::make(init, |ptr| {
                    // SAFETY: `RawRuntimePayload` matches `RustModCustomData` size (8) and alignment (4).
                    unsafe {
                        emut.insert_by_id(comp_id, ptr);
                    }
                });
                if i == 0 {
                    e0 = Some(id);
                }
            }

            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            let ptr = app.world().entity(e0.unwrap()).get_by_id(comp_id).unwrap();
            // SAFETY: Bytes stored under `comp_id` have layout `(f32, u32)`.
            let d0 = unsafe { *(ptr.as_ptr().cast::<RawRuntimePayload>()) };
            assert_eq!(d0.secondary, 1 + TOTAL_TICKS as u32);

            app.unload_mod(mod_id).expect("unload rust mod");
            res
        };

        // 5. Runtime-Registered Dynamic Component — Pure C99 `.dylib` (`CModCustomData`)
        let (rt_c_med_ms, rt_c_tput, rt_c_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let mod_id = app.load_mod(&c_dylib).expect("load c99 mod");
            let comp_id = lookup_dynamic_component_id(
                app.world().resource::<ModRegistry>(),
                "asymmetric_c_mod::CModCustomData",
                "CModCustomData",
            );

            let mut e0 = None;
            for i in 0..entity_count {
                let id = app.world_mut().spawn_empty().id();
                let init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let mut emut = app.world_mut().entity_mut(id);
                OwningPtr::make(init, |ptr| {
                    // SAFETY: `RawRuntimePayload` matches `CModCustomData` size (8) and alignment (4).
                    unsafe {
                        emut.insert_by_id(comp_id, ptr);
                    }
                });
                if i == 0 {
                    e0 = Some(id);
                }
            }

            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            let ptr = app.world().entity(e0.unwrap()).get_by_id(comp_id).unwrap();
            // SAFETY: Bytes stored under `comp_id` have layout `(f32, i32)`.
            let d0 = unsafe { *(ptr.as_ptr().cast::<RawRuntimePayload>()) };
            assert_eq!(d0.secondary, 1 + TOTAL_TICKS as u32);

            app.unload_mod(mod_id).expect("unload c99 mod");
            res
        };

        // 6. Parallel Disjoint Runtime Components — Rust + C99 Concurrent Multi-Threaded Execution
        let (rt_par_med_ms, rt_par_tput, rt_par_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let r_id = app.load_mod(&rust_dylib).expect("load rust mod");
            let c_id = app.load_mod(&c_dylib).expect("load c99 mod");
            let (rust_comp_id, c_comp_id) = {
                let reg = app.world().resource::<ModRegistry>();
                (
                    lookup_dynamic_component_id(
                        reg,
                        "asymmetric_rust_mod::RustModCustomData",
                        "RustModCustomData",
                    ),
                    lookup_dynamic_component_id(
                        reg,
                        "asymmetric_c_mod::CModCustomData",
                        "CModCustomData",
                    ),
                )
            };

            for i in 0..entity_count {
                let id = app.world_mut().spawn_empty().id();
                let r_init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let c_init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let mut emut = app.world_mut().entity_mut(id);
                OwningPtr::make(r_init, |ptr| {
                    // SAFETY: Matches `RustModCustomData` layout.
                    unsafe {
                        emut.insert_by_id(rust_comp_id, ptr);
                    }
                });
                OwningPtr::make(c_init, |ptr| {
                    // SAFETY: Matches `CModCustomData` layout.
                    unsafe {
                        emut.insert_by_id(c_comp_id, ptr);
                    }
                });
            }

            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            app.unload_mod(r_id).expect("unload rust mod");
            app.unload_mod(c_id).expect("unload c99 mod");
            res
        };

        println!(
            "  Native Bevy (8B struct)        | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            nat_med_ms, nat_tput, nat_ns
        );
        println!(
            "  Runtime Dynamic (Rust cdylib)  | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            rt_rust_med_ms, rt_rust_tput, rt_rust_ns
        );
        println!(
            "  Runtime Dynamic (C99 dylib)    | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            rt_c_med_ms, rt_c_tput, rt_c_ns
        );
        println!(
            "  Runtime Parallel (Rust + C99)  | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            rt_par_med_ms, rt_par_tput, rt_par_ns
        );
        println!(
            "  Native Bevy (Pos + Vel)        | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            nat_kin_med_ms, nat_kin_tput, nat_kin_ns
        );
        println!(
            "  Static ModComponent VTable     | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            stat_med_ms, stat_tput, stat_ns
        );
    }

    println!("\n--- Archetype Fragmentation Efficiency (10,000 entities, {ROUNDS} frames) ---");
    for &num_archetypes in &[1_usize, 4, 16] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .expect("register Position");
        app.register_mod_component::<Velocity>(
            ComponentOptions::new().with_raw_insert().with_default(),
        )
        .expect("register Velocity");
        for i in 0..10_000_usize {
            let bucket = i % num_archetypes;
            let mut emut = app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 2.0,
                    z: 0.0,
                },
            ));
            if bucket & 1 != 0 {
                emut.insert(FragTagA(i as u32));
            }
            if bucket & 2 != 0 {
                emut.insert(FragTagB(i as u32));
            }
            if bucket & 4 != 0 {
                emut.insert(FragTagC(i as u32));
            }
            if bucket & 8 != 0 {
                emut.insert(FragTagD(i as u32));
            }
        }
        let mod_id = app.load_mod(&rust_dylib).expect("load rust mod");
        let (frag_med_ms, frag_tput, frag_ns) =
            measure_app_rounds(&mut app, WARMUP, ROUNDS, 10_000);
        app.unload_mod(mod_id).expect("unload rust mod");
        println!(
            "  {:>2}-way Fragmented Archetypes   | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            num_archetypes, frag_med_ms, frag_tput, frag_ns
        );
    }

    println!("\nMicrobenchmark completed successfully.");
}
