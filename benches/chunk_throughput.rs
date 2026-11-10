//! Microbenchmark: Chunk Iteration Throughput
//! Standard library Instant based, zero external dependencies.

use bevy_app::App;
use bevy_ecs::component::Component;
use host_runtime::{AbiModPlugin, ModAppExt};
use mod_derive::ModExposable;
use std::path::PathBuf;
use std::time::Instant;

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

fn main() {
    let (rust_dylib, _c_dylib) = get_dylib_paths();
    if !rust_dylib.exists() {
        eprintln!(
            "Guest mod dynamic library not found at {:?}. Please run `cargo build --manifest-path tests/asymmetric_rust_mod/Cargo.toml --release` first.",
            rust_dylib
        );
        std::process::exit(1);
    }

    println!("================================================================================");
    println!(" MICROBENCHMARK: Chunk Iteration Throughput");
    println!("================================================================================");

    for &entity_count in &[1_000, 10_000, 100_000] {
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

        // Warmup runs
        for _ in 0..5 {
            app.update();
        }

        // Benchmark rounds
        const ROUNDS: usize = 20;
        let mut round_durations = Vec::with_capacity(ROUNDS);

        for _ in 0..ROUNDS {
            let start = Instant::now();
            app.update();
            round_durations.push(start.elapsed());
        }

        app.unload_mod(mod_id).expect("unload rust mod");

        round_durations.sort();
        let median = round_durations[ROUNDS / 2];
        let min = round_durations[0];
        let max = round_durations[ROUNDS - 1];

        let total_time: std::time::Duration = round_durations.iter().sum();
        let avg_secs = total_time.as_secs_f64() / (ROUNDS as f64);
        let entities_per_sec = (entity_count as f64) / avg_secs;
        let ns_per_entity = (total_time.as_nanos() as f64) / ((entity_count * ROUNDS) as f64);

        println!(
            "[{:>6} entities] Avg: {:>8.4} ms | Median: {:>8.4} ms | Min: {:>8.4} ms | Max: {:>8.4} ms",
            entity_count,
            avg_secs * 1000.0,
            median.as_secs_f64() * 1000.0,
            min.as_secs_f64() * 1000.0,
            max.as_secs_f64() * 1000.0
        );
        println!(
            "                 Throughput: {:>12.2} entities/sec | Latency: {:>8.2} ns/entity",
            entities_per_sec, ns_per_entity
        );
        println!(
            "--------------------------------------------------------------------------------"
        );
    }

    println!("Microbenchmark completed successfully.");
}
