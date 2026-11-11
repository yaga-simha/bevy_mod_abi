//! Microbenchmark: Chunk Iteration Throughput
//! Compares Native Bevy ECS, Static `#[derive(ModExposable)]` C-ABI VTable access,
//! and Runtime-Registered Dynamic Components (Rust `.cdylib` & C99 `.dylib`).
//! Standard library `Instant` based, zero external benchmark harness dependencies.

#![deny(unsafe_op_in_unsafe_fn)]

use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::system::Query;
use host_runtime::{AbiModPlugin, ModAppExt, ModRegistry};
use mod_derive::ModExposable;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Native Bevy baseline matching the 8-byte `(f32, u32)` payload of `RustModCustomData` / `CModCustomData`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
pub struct NativeCustomData {
    pub power: f32,
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
    for mut item in query.iter_mut() {
        item.power += 10.0;
        item.shield += 1;
    }
}

fn native_kinematics_system(mut query: Query<(&mut Position, &Velocity)>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
        pos.z += vel.z;
    }
}

fn get_dylib_paths() -> (PathBuf, PathBuf) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = if manifest_dir.ends_with("host_runtime") {
        manifest_dir
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf()
    } else {
        manifest_dir
    };

    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };

    let rust_dylib = root_dir
        .join("tests/asymmetric_rust_mod/target/release")
        .join(format!("{prefix}asymmetric_rust_mod.{ext}"));
    let c_dylib = root_dir
        .join("tests/asymmetric_c_mod")
        .join(format!("{prefix}asymmetric_c_mod.{ext}"));

    (rust_dylib, c_dylib)
}

fn measure_app_rounds(app: &mut App, warmup: usize, rounds: usize, entity_count: usize) -> (f64, f64, f64) {
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
    if !rust_dylib.exists() {
        eprintln!(
            "Rust guest mod dynamic library not found at {:?}. Run `cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release` first.",
            rust_dylib
        );
        std::process::exit(1);
    }
    if !c_dylib.exists() {
        eprintln!(
            "C99 guest mod dynamic library not found at {:?}. Run `make -C tests/asymmetric_c_mod` first.",
            c_dylib
        );
        std::process::exit(1);
    }

    const WARMUP: usize = 5;
    const ROUNDS: usize = 30;
    const TOTAL_TICKS: usize = WARMUP + ROUNDS;

    println!("==========================================================================================");
    println!(" MICROBENCHMARK: Runtime-Registered vs. Static ModExposable vs. Native Bevy Throughput");
    println!("==========================================================================================");

    for &entity_count in &[1_000_usize, 10_000, 100_000] {
        println!("\n--- Scale: {:>6} Entities ({ROUNDS} measured frames) ---", entity_count);

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

        // 3. Static `#[derive(ModExposable)]` C-ABI VTable Access (Position + Velocity)
        let (stat_med_ms, stat_tput, stat_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            app.register_mod_component::<Position>();
            app.register_mod_component::<Velocity>();
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
            let mod_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
            let res = measure_app_rounds(&mut app, WARMUP, ROUNDS, entity_count);
            app.unload_mod(mod_id).expect("unload rust mod");
            res
        };

        // 4. Runtime-Registered Dynamic Component — Rust `.cdylib` (`RustModCustomData`)
        let (rt_rust_med_ms, rt_rust_tput, rt_rust_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let mod_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
            let comp_id = app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("RustModCustomData")
                .expect("RustModCustomData registered at runtime")
                .component_id;

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
            let d0 = unsafe { *(ptr.as_ptr() as *const RawRuntimePayload) };
            assert_eq!(d0.secondary, 1 + TOTAL_TICKS as u32);

            app.unload_mod(mod_id).expect("unload rust mod");
            res
        };

        // 5. Runtime-Registered Dynamic Component — Pure C99 `.dylib` (`CModCustomData`)
        let (rt_c_med_ms, rt_c_tput, rt_c_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let mod_id = app.load_mod_tracked(&c_dylib).expect("load c99 mod");
            let comp_id = app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("CModCustomData")
                .expect("CModCustomData registered at runtime")
                .component_id;

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
            let d0 = unsafe { *(ptr.as_ptr() as *const RawRuntimePayload) };
            assert_eq!(d0.secondary, 1 + TOTAL_TICKS as u32);

            app.unload_mod(mod_id).expect("unload c99 mod");
            res
        };

        // 6. Parallel Disjoint Runtime Components — Rust + C99 Concurrent Multi-Threaded Execution
        let (rt_par_med_ms, rt_par_tput, rt_par_ns) = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let r_id = app.load_mod_tracked(&rust_dylib).expect("load rust mod");
            let c_id = app.load_mod_tracked(&c_dylib).expect("load c99 mod");
            let (rust_comp_id, c_comp_id) = {
                let reg = app.world().resource::<ModRegistry>();
                (
                    reg.get_by_name("RustModCustomData").unwrap().component_id,
                    reg.get_by_name("CModCustomData").unwrap().component_id,
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
            "  Static ModExposable VTable     | Median: {:>7.4} ms | {:>13.2} ent/s | {:>6.2} ns/ent",
            stat_med_ms, stat_tput, stat_ns
        );
    }

    println!("\nMicrobenchmark completed successfully.");
}
