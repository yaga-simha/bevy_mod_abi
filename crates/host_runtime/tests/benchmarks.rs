#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

mod common;

use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::With;
use bevy_ecs::system::Query;
use common::*;
use host_runtime::{AbiModPlugin, ModAppExt, ModRegistry};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

#[derive(Component, Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct NativeCustomData {
    power: f32,
    shield: u32,
}

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

// ============================================================================
// Milestone 4: Performance Benchmarking Suite
// ============================================================================

struct BenchmarkMeasurement {
    name: String,
    scale: usize,
    topology: String,
    iterations: usize,
    elapsed_millis: f64,
    throughput_entities_per_sec: f64,
    ns_per_entity: f64,
    native_ns_per_entity: Option<f64>,
    abi_overhead_percentage: Option<f64>,
}

fn native_kinematics_system(mut query: Query<(&mut Position, &Velocity), With<Position>>) {
    for (mut pos, vel) in query.iter_mut() {
        pos.x += vel.x;
        pos.y += vel.y;
        pos.z += vel.z;
    }
}

fn format_json_report(measurements: &[BenchmarkMeasurement]) -> String {
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str("  \"timestamp\": \"2026-11-10T16:45:00Z\",\n");
    json.push_str("  \"environment\": {\n");
    json.push_str(&format!("    \"os\": \"{}\",\n", std::env::consts::OS));
    json.push_str(&format!("    \"arch\": \"{}\",\n", std::env::consts::ARCH));
    json.push_str(&format!(
        "    \"logical_cores\": {}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    ));
    json.push_str("  },\n");
    json.push_str("  \"results\": [\n");

    for (i, m) in measurements.iter().enumerate() {
        json.push_str("    {\n");
        json.push_str(&format!("      \"benchmark_name\": \"{}\",\n", m.name));
        json.push_str(&format!("      \"entity_scale\": {},\n", m.scale));
        json.push_str(&format!(
            "      \"archetype_topology\": \"{}\",\n",
            m.topology
        ));
        json.push_str(&format!("      \"iterations\": {},\n", m.iterations));
        json.push_str(&format!(
            "      \"elapsed_millis\": {:.4},\n",
            m.elapsed_millis
        ));
        json.push_str(&format!(
            "      \"throughput_entities_per_sec\": {:.2},\n",
            m.throughput_entities_per_sec
        ));
        json.push_str(&format!(
            "      \"ns_per_entity\": {:.4},\n",
            m.ns_per_entity
        ));

        match m.native_ns_per_entity {
            Some(v) => json.push_str(&format!("      \"native_ns_per_entity\": {:.4},\n", v)),
            None => json.push_str("      \"native_ns_per_entity\": null,\n"),
        }
        match m.abi_overhead_percentage {
            Some(v) => json.push_str(&format!("      \"abi_overhead_percentage\": {:.2}\n", v)),
            None => json.push_str("      \"abi_overhead_percentage\": null\n"),
        }

        if i + 1 < measurements.len() {
            json.push_str("    },\n");
        } else {
            json.push_str("    }\n");
        }
    }

    json.push_str("  ]\n");
    json.push_str("}\n");
    json
}

fn write_json_report(json_content: &str) {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();

    let target_dirs = [
        root_dir.join("target"),
        root_dir.join("target/benchmarks"),
        manifest_dir.join("target"),
    ];

    for dir in &target_dirs {
        let _ = fs::create_dir_all(dir);
        let file_path = dir.join("benchmark_results.json");
        let _ = fs::write(&file_path, json_content);
    }
}

#[test]
fn test_performance_benchmarking_suite() {
    let (rust_dylib, c_dylib) = ensure_guest_mods_built();
    reset_rust_mod_panic(&rust_dylib);

    let mut measurements = Vec::new();

    println!("\n================================================================================");
    println!(" RUNNING BEVY ABI MOD PERFORMANCE BENCHMARKING SUITE");
    println!("================================================================================");

    // ------------------------------------------------------------------------
    // Benchmark 1, 2, 3: Chunk Iteration Throughput at 1k, 10k, 100k Entities
    // ------------------------------------------------------------------------
    for &scale in &[1_000, 10_000, 100_000] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        for i in 0..scale {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                },
            ));
        }

        let r_id = app
            .load_mod_tracked(&rust_dylib)
            .expect("load rust mod for bench");

        // Warmup (3 ticks)
        for _ in 0..3 {
            app.update();
        }

        let iterations = if scale <= 10_000 { 50 } else { 20 };
        let start = Instant::now();
        for _ in 0..iterations {
            app.update();
        }
        let elapsed = start.elapsed();

        let total_entities = scale * iterations;
        let elapsed_secs = elapsed.as_secs_f64();
        let throughput = (total_entities as f64) / elapsed_secs;
        let ns_per_entity = (elapsed.as_nanos() as f64) / (total_entities as f64);

        println!(
            "[{scale:>6} Entities] Throughput: {:>12.2} entities/sec | Latency: {:>8.2} ns/entity | Elapsed: {:>8.4} ms",
            throughput, ns_per_entity, elapsed_secs * 1000.0
        );

        measurements.push(BenchmarkMeasurement {
            name: format!("chunk_iteration_{scale}"),
            scale,
            topology: "monolithic".to_string(),
            iterations,
            elapsed_millis: elapsed_secs * 1000.0,
            throughput_entities_per_sec: throughput,
            ns_per_entity,
            native_ns_per_entity: None,
            abi_overhead_percentage: None,
        });

        app.unload_mod(r_id).unwrap();
    }

    // ------------------------------------------------------------------------
    // Benchmark 4: C-ABI Boundary Overhead vs Equivalent Native Bevy System
    // ------------------------------------------------------------------------
    const SCALE_OVERHEAD: usize = 10_000;
    const ITERS_OVERHEAD: usize = 50;

    // A. Native Bevy System Measurement
    let native_ns = {
        let mut app = App::new();
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        for i in 0..SCALE_OVERHEAD {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                },
            ));
        }

        app.add_systems(Update, native_kinematics_system);

        // Warmup
        for _ in 0..3 {
            app.update();
        }

        let start = Instant::now();
        for _ in 0..ITERS_OVERHEAD {
            app.update();
        }
        let elapsed = start.elapsed();

        let total = SCALE_OVERHEAD * ITERS_OVERHEAD;
        (elapsed.as_nanos() as f64) / (total as f64)
    };

    // B. C-ABI Boundary Measurement (asymmetric_rust_mod kinematics)
    let (abi_throughput, abi_ns, abi_millis) = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        for i in 0..SCALE_OVERHEAD {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                },
            ));
        }

        let r_id = app.load_mod_tracked(&rust_dylib).unwrap();

        // Warmup
        for _ in 0..3 {
            app.update();
        }

        let start = Instant::now();
        for _ in 0..ITERS_OVERHEAD {
            app.update();
        }
        let elapsed = start.elapsed();

        app.unload_mod(r_id).unwrap();

        let total = SCALE_OVERHEAD * ITERS_OVERHEAD;
        let secs = elapsed.as_secs_f64();
        (
            (total as f64) / secs,
            (elapsed.as_nanos() as f64) / (total as f64),
            secs * 1000.0,
        )
    };

    let overhead_pct = ((abi_ns - native_ns) / native_ns) * 100.0;

    println!("\n--- C-ABI Boundary Overhead (10,000 entities) ---");
    println!("  Native Bevy Query Loop: {:>8.2} ns/entity", native_ns);
    println!("  C-ABI Chunk Iterator:   {:>8.2} ns/entity", abi_ns);
    println!("  Boundary Delta Overhead:{:>8.2}%", overhead_pct);

    measurements.push(BenchmarkMeasurement {
        name: "c_abi_overhead_vs_native_10k".to_string(),
        scale: SCALE_OVERHEAD,
        topology: "monolithic".to_string(),
        iterations: ITERS_OVERHEAD,
        elapsed_millis: abi_millis,
        throughput_entities_per_sec: abi_throughput,
        ns_per_entity: abi_ns,
        native_ns_per_entity: Some(native_ns),
        abi_overhead_percentage: Some(overhead_pct),
    });

    // ------------------------------------------------------------------------
    // Benchmark 5: Archetype Fragmentation Efficiency (Monolithic vs 4-way vs 16-way)
    // ------------------------------------------------------------------------
    const SCALE_FRAG: usize = 10_000;
    const ITERS_FRAG: usize = 30;

    for &num_archetypes in &[1, 4, 16] {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();
        app.register_mod_component::<CombatHealth>();
        app.register_mod_component::<NameTag>();
        app.register_mod_component::<Mass>();
        app.register_mod_component::<Armor>();

        spawn_heterogeneous_archetypes(app.world_mut(), SCALE_FRAG, num_archetypes);

        let r_id = app.load_mod_tracked(&rust_dylib).unwrap();

        for _ in 0..3 {
            app.update();
        }

        let start = Instant::now();
        for _ in 0..ITERS_FRAG {
            app.update();
        }
        let elapsed = start.elapsed();

        app.unload_mod(r_id).unwrap();

        let total = SCALE_FRAG * ITERS_FRAG;
        let secs = elapsed.as_secs_f64();
        let throughput = (total as f64) / secs;
        let ns_per_entity = (elapsed.as_nanos() as f64) / (total as f64);

        println!(
            "[{:>2}-way Archetype Fragmentation] Throughput: {:>12.2} entities/sec | Latency: {:>8.2} ns/entity",
            num_archetypes, throughput, ns_per_entity
        );

        measurements.push(BenchmarkMeasurement {
            name: format!("archetype_fragmentation_{num_archetypes}_way"),
            scale: SCALE_FRAG,
            topology: format!("{num_archetypes}_way_fragmented"),
            iterations: ITERS_FRAG,
            elapsed_millis: secs * 1000.0,
            throughput_entities_per_sec: throughput,
            ns_per_entity,
            native_ns_per_entity: None,
            abi_overhead_percentage: None,
        });
    }

    // ------------------------------------------------------------------------
    // Benchmark 6: Simultaneous C and Rust Mod Workload
    // ------------------------------------------------------------------------
    {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        app.register_mod_component::<Position>();
        app.register_mod_component::<Velocity>();

        for i in 0..10_000 {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Velocity {
                    x: 1.0,
                    y: 1.0,
                    z: 1.0,
                },
            ));
        }

        let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
        let c_id = app.load_mod_tracked(&c_dylib).unwrap();

        for _ in 0..3 {
            app.update();
        }

        let start = Instant::now();
        for _ in 0..30 {
            app.update();
        }
        let elapsed = start.elapsed();

        app.unload_mod(r_id).unwrap();
        app.unload_mod(c_id).unwrap();

        let total = 10_000 * 30;
        let secs = elapsed.as_secs_f64();
        let throughput = (total as f64) / secs;
        let ns = (elapsed.as_nanos() as f64) / (total as f64);

        println!(
            "[Dual C+Rust Concurrent Workload] Throughput: {:>12.2} entities/sec | Latency: {:>8.2} ns/entity",
            throughput, ns
        );

        measurements.push(BenchmarkMeasurement {
            name: "concurrent_c_and_rust_workload_10k".to_string(),
            scale: 10_000,
            topology: "monolithic".to_string(),
            iterations: 30,
            elapsed_millis: secs * 1000.0,
            throughput_entities_per_sec: throughput,
            ns_per_entity: ns,
            native_ns_per_entity: None,
            abi_overhead_percentage: None,
        });
    }

    // ------------------------------------------------------------------------
    // Benchmark 7: Runtime-Registered Dynamic Components vs Native Bevy (10k & 100k)
    // ------------------------------------------------------------------------
    println!("\n--- Runtime-Registered Dynamic Components vs Native Bevy ---");
    for &scale in &[10_000_usize, 100_000] {
        let iterations = if scale <= 10_000 { 50 } else { 20 };
        let total = scale * iterations;

        // 7A. Native Bevy baseline (8-byte struct: power + shield)
        let native_custom_ns = {
            let mut app = App::new();
            for i in 0..scale {
                app.world_mut().spawn(NativeCustomData {
                    power: i as f32,
                    shield: 1,
                });
            }
            app.add_systems(Update, native_custom_system);
            for _ in 0..3 {
                app.update();
            }
            let start = Instant::now();
            for _ in 0..iterations {
                app.update();
            }
            let elapsed = start.elapsed();
            (elapsed.as_nanos() as f64) / (total as f64)
        };

        // 7B. Rust Runtime-Registered Dynamic Component (`RustModCustomData`)
        {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
            let rust_comp_id = app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("RustModCustomData")
                .unwrap()
                .component_id;

            for i in 0..scale {
                let id = app.world_mut().spawn_empty().id();
                let init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let mut emut = app.world_mut().entity_mut(id);
                OwningPtr::make(init, |ptr| {
                    // SAFETY: `RawRuntimePayload` matches `RustModCustomData` layout (size 8, align 4).
                    unsafe {
                        emut.insert_by_id(rust_comp_id, ptr);
                    }
                });
            }

            for _ in 0..3 {
                app.update();
            }
            let start = Instant::now();
            for _ in 0..iterations {
                app.update();
            }
            let elapsed = start.elapsed();
            app.unload_mod(r_id).unwrap();

            let secs = elapsed.as_secs_f64();
            let throughput = (total as f64) / secs;
            let ns = (elapsed.as_nanos() as f64) / (total as f64);
            let overhead_pct = ((ns - native_custom_ns) / native_custom_ns) * 100.0;

            println!(
                "[Runtime Rust Dynamic {scale:>6}] Throughput: {:>12.2} ent/sec | {:>6.2} ns/ent (Native: {:>6.2} ns/ent, Delta: {:>+6.2}%)",
                throughput, ns, native_custom_ns, overhead_pct
            );

            measurements.push(BenchmarkMeasurement {
                name: format!("runtime_dynamic_component_rust_{scale}"),
                scale,
                topology: "monolithic_soa".to_string(),
                iterations,
                elapsed_millis: secs * 1000.0,
                throughput_entities_per_sec: throughput,
                ns_per_entity: ns,
                native_ns_per_entity: Some(native_custom_ns),
                abi_overhead_percentage: Some(overhead_pct),
            });
        }

        // 7C. C99 Runtime-Registered Dynamic Component (`CModCustomData`)
        {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let c_id = app.load_mod_tracked(&c_dylib).unwrap();
            let c_comp_id = app
                .world()
                .resource::<ModRegistry>()
                .get_by_name("CModCustomData")
                .unwrap()
                .component_id;

            for i in 0..scale {
                let id = app.world_mut().spawn_empty().id();
                let init = RawRuntimePayload {
                    primary: i as f32,
                    secondary: 1,
                };
                let mut emut = app.world_mut().entity_mut(id);
                OwningPtr::make(init, |ptr| {
                    // SAFETY: `RawRuntimePayload` matches `CModCustomData` layout (size 8, align 4).
                    unsafe {
                        emut.insert_by_id(c_comp_id, ptr);
                    }
                });
            }

            for _ in 0..3 {
                app.update();
            }
            let start = Instant::now();
            for _ in 0..iterations {
                app.update();
            }
            let elapsed = start.elapsed();
            app.unload_mod(c_id).unwrap();

            let secs = elapsed.as_secs_f64();
            let throughput = (total as f64) / secs;
            let ns = (elapsed.as_nanos() as f64) / (total as f64);
            let overhead_pct = ((ns - native_custom_ns) / native_custom_ns) * 100.0;

            println!(
                "[Runtime C99 Dynamic  {scale:>6}] Throughput: {:>12.2} ent/sec | {:>6.2} ns/ent (Native: {:>6.2} ns/ent, Delta: {:>+6.2}%)",
                throughput, ns, native_custom_ns, overhead_pct
            );

            measurements.push(BenchmarkMeasurement {
                name: format!("runtime_dynamic_component_c99_{scale}"),
                scale,
                topology: "monolithic_soa".to_string(),
                iterations,
                elapsed_millis: secs * 1000.0,
                throughput_entities_per_sec: throughput,
                ns_per_entity: ns,
                native_ns_per_entity: Some(native_custom_ns),
                abi_overhead_percentage: Some(overhead_pct),
            });
        }

        // 7D. Parallel Multi-Mod Runtime Components (`RustModCustomData` + `CModCustomData`)
        {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let r_id = app.load_mod_tracked(&rust_dylib).unwrap();
            let c_id = app.load_mod_tracked(&c_dylib).unwrap();
            let (rust_comp_id, c_comp_id) = {
                let reg = app.world().resource::<ModRegistry>();
                (
                    reg.get_by_name("RustModCustomData").unwrap().component_id,
                    reg.get_by_name("CModCustomData").unwrap().component_id,
                )
            };

            for i in 0..scale {
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

            for _ in 0..3 {
                app.update();
            }
            let start = Instant::now();
            for _ in 0..iterations {
                app.update();
            }
            let elapsed = start.elapsed();
            app.unload_mod(r_id).unwrap();
            app.unload_mod(c_id).unwrap();

            let secs = elapsed.as_secs_f64();
            let throughput = (total as f64) / secs;
            let ns = (elapsed.as_nanos() as f64) / (total as f64);

            println!(
                "[Runtime Dual Parallel {scale:>6}] Throughput: {:>12.2} ent/sec | {:>6.2} ns/ent",
                throughput, ns
            );

            measurements.push(BenchmarkMeasurement {
                name: format!("runtime_dynamic_component_parallel_dual_{scale}"),
                scale,
                topology: "parallel_disjoint_soa".to_string(),
                iterations,
                elapsed_millis: secs * 1000.0,
                throughput_entities_per_sec: throughput,
                ns_per_entity: ns,
                native_ns_per_entity: Some(native_custom_ns),
                abi_overhead_percentage: None,
            });
        }
    }

    println!("================================================================================\n");

    // Serialize and write JSON report
    let json_report = format_json_report(&measurements);
    write_json_report(&json_report);

    // Verification: ensure report file was written and is non-empty
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = manifest_dir.parent().unwrap().parent().unwrap();
    let json_file = root_dir.join("target/benchmark_results.json");
    assert!(
        json_file.exists(),
        "target/benchmark_results.json must exist"
    );
    let read_back = fs::read_to_string(&json_file).expect("Failed to read back json report");
    assert!(
        read_back.contains("\"results\":"),
        "JSON report must contain results"
    );
    assert!(
        read_back.contains("\"chunk_iteration_100000\""),
        "JSON report must contain 100k scale"
    );
    assert!(
        read_back.contains("\"runtime_dynamic_component_rust_100000\""),
        "JSON report must contain 100k runtime Rust dynamic component benchmark"
    );
    assert!(
        read_back.contains("\"runtime_dynamic_component_c99_100000\""),
        "JSON report must contain 100k runtime C99 dynamic component benchmark"
    );
}
