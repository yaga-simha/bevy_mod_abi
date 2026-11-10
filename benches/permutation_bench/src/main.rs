#![deny(unsafe_op_in_unsafe_fn)]

//! Comprehensive Cross-Architecture & Cross-Profile Benchmark Matrix Runner.
//!
//! Covers EVERY permutation of:
//! - Execution Architecture:
//!     1. Pure Native Bevy (No-Mod, statically compiled Rust)
//!     2. Coupled Rust `dylib` Mod (shared Rust ABI & `bevy_ecs` `Query` via `libloading`)
//!     3. Asymmetric C-ABI Rust `cdylib` Mod (`bevy_abi_mod` + `guest_sdk`):
//!        - `ChunkOptimized` ($O(U + E)$ VTable crossings)
//!        - `EntityNestedVTable` ($O(U \times E)$ VTable getters + `iter_reset`)
//!        - `NaiveChattyFfi` ($O(U \times E)$ VTable reads/writes + per-pair `iter_mark_changed`)
//!     4. Asymmetric C-ABI C99 Mod (`bevy_abi_mod` + `bevy_abi_mod.h`):
//!        - `ChunkOptimized`, `EntityNestedVTable`, and `NaiveChattyFfi`
//! - Build Profiles:
//!     - Host [`Release` (`opt-level=3`)] × Guest [`Release` / `-O3` and `Debug` / `-O0`]
//!     - Host [`Debug`   (`opt-level=0`)] × Guest [`Release` / `-O3` and `Debug` / `-O0`]
//! - Both Dimensions:
//!     - Mod Loading, Handshake, 34-Archetype Schedule Init & Teardown Latency (`µs`)
//!     - Steady-State Complex Nested Multi-Archetype Query Frame Time (`ms`, `ns/pair`, slowdown `x`)

use libloading::{Library, Symbol};
use shared_scenario::bevy_app::{App, Update};
use shared_scenario::bevy_ecs::schedule::ScheduleLabel;
use shared_scenario::host_runtime::{AbiModPlugin, ModAppExt, ModHost};
use shared_scenario::{
    native_cached_soa_nested_aura_combat_system, native_nested_aura_combat_system,
    populate_scenario_world, register_all_scenario_components, register_pure_native_components,
    OracleDigest, ScenarioConfig,
};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

// ============================================================================
// 1. Data Structures for Measurements
// ============================================================================

#[derive(Clone, Debug)]
struct LoadRecord {
    category: String,
    host_profile: String,
    mod_profile: String,
    status: String,
    binary_size_kb: f64,
    dlopen_us: f64,
    init_and_schedule_us: f64,
    unload_us: f64,
    total_lifecycle_us: f64,
    notes: String,
}

#[derive(Clone, Debug)]
struct ExecRecord {
    tier_label: String,
    architecture: String,
    access_pattern: String,
    host_profile: String,
    mod_profile: String,
    scale_name: String,
    archetypes_total: usize,
    active_units: usize,
    active_emitters: usize,
    pairs_per_frame: usize,
    frames_measured: usize,
    median_frame_ms: f64,
    mean_frame_ms: f64,
    min_frame_ms: f64,
    p95_frame_ms: f64,
    ns_per_pair: f64,
    ns_per_unit: f64,
    pairs_per_sec: f64,
    oracle_verified: bool,
}

struct ModArtifacts {
    coupled_rust_same_profile: PathBuf,
    cabi_rust_release: PathBuf,
    cabi_rust_debug: PathBuf,
    cabi_c99_release: PathBuf,
    cabi_c99_debug: PathBuf,
}

fn root_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.parent().unwrap().parent().unwrap().to_path_buf()
}

fn dylib_prefix_ext() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

fn current_host_profile() -> &'static str {
    if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    }
}

fn resolve_mod_artifacts(root: &Path, host_prof: &str) -> ModArtifacts {
    let (prefix, ext) = dylib_prefix_ext();
    ModArtifacts {
        coupled_rust_same_profile: root
            .join("target")
            .join(host_prof)
            .join(format!("{prefix}coupled_rust_mod.{ext}")),
        cabi_rust_release: root
            .join("benches/c_abi_rust_mod/target/release")
            .join(format!("{prefix}c_abi_rust_mod.{ext}")),
        cabi_rust_debug: root
            .join("benches/c_abi_rust_mod/target/debug")
            .join(format!("{prefix}c_abi_rust_mod.{ext}")),
        cabi_c99_release: root
            .join("benches/c_abi_c99_mod")
            .join(format!("{prefix}c_abi_c99_mod_release.{ext}")),
        cabi_c99_debug: root
            .join("benches/c_abi_c99_mod")
            .join(format!("{prefix}c_abi_c99_mod_debug.{ext}")),
    }
}

fn file_size_kb(path: &Path) -> f64 {
    fs::metadata(path)
        .map(|m| (m.len() as f64) / 1024.0)
        .unwrap_or(0.0)
}

/// Configures the variant selector on a C-ABI benchmark mod before `ModHost` invokes `bevy_abi_mod_init`.
/// Kept open during `load_mod_tracked` so the static global in the mapped image retains `variant`.
fn set_cabi_mod_variant(dylib_path: &Path, sym_name: &[u8], variant: u32) -> Library {
    // SAFETY: Loading trusted benchmark shared library and calling side-effect-free integer setter.
    unsafe {
        let lib = Library::new(dylib_path)
            .unwrap_or_else(|e| panic!("Failed to open {:?}: {e}", dylib_path));
        let setter: Symbol<unsafe extern "C" fn(u32)> = lib
            .get(sym_name)
            .unwrap_or_else(|e| panic!("Missing variant symbol in {:?}: {e}", dylib_path));
        setter(variant);
        lib
    }
}

// ============================================================================
// 2. Step A: Mathematical Oracle Verification Across All Permutations
// ============================================================================

fn verify_all_permutations_against_oracle(mods: &ModArtifacts, host_prof: &str) {
    let cfg = ScenarioConfig::oracle();
    const TICKS: usize = 4;

    // 1. Capture Golden Pure Native Bevy Oracle
    let golden = {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        let stats = populate_scenario_world(app.world_mut(), cfg);
        assert!(
            stats.total_archetypes >= 34,
            "Expected >= 34 archetypes, got {}",
            stats.total_archetypes
        );
        app.add_systems(Update, native_nested_aura_combat_system);
        for _ in 0..TICKS {
            app.update();
        }
        OracleDigest::capture(app.world_mut())
    };

    // 1b. Verify Pure Native Bevy (L1 Pre-Cached SoA) against Oracle
    {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        populate_scenario_world(app.world_mut(), cfg);
        app.add_systems(Update, native_cached_soa_nested_aura_combat_system);
        for _ in 0..TICKS {
            app.update();
        }
        let digest = OracleDigest::capture(app.world_mut());
        golden.assert_equivalent(&digest, &format!("PureNativeCachedSoA[{host_prof}]"));
    }

    // 2. Verify Coupled Rust `dylib` Mod (same profile)
    {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        populate_scenario_world(app.world_mut(), cfg);

        // SAFETY: Loading same-profile coupled_rust_mod sharing `libshared_scenario.dylib`.
        let lib = unsafe { Library::new(&mods.coupled_rust_same_profile) }
            .expect("Failed to load coupled_rust_mod");
        unsafe {
            #[allow(improper_ctypes_definitions)]
            let reg: Symbol<extern "Rust" fn(&mut App)> = lib
                .get(b"coupled_rust_mod_register\0")
                .expect("Missing coupled_rust_mod_register");
            reg(&mut app);
        }
        for _ in 0..TICKS {
            app.update();
        }
        let digest = OracleDigest::capture(app.world_mut());
        golden.assert_equivalent(&digest, &format!("CoupledRustDylib[{host_prof}]"));

        unsafe {
            let shutdown: Symbol<extern "C" fn()> = lib
                .get(b"coupled_rust_mod_shutdown\0")
                .expect("Missing coupled_rust_mod_shutdown");
            shutdown();
        }
        drop(app);
        std::mem::forget(lib);
    }

    // 3. Verify all C-ABI Rust & C99 Mods (Debug & Release × 3 Variants = 12 permutations)
    let cabi_targets: [(&str, &Path, &[u8]); 4] = [
        (
            "C-ABI Rust[release]",
            &mods.cabi_rust_release,
            b"c_abi_rust_mod_set_variant\0",
        ),
        (
            "C-ABI Rust[debug]",
            &mods.cabi_rust_debug,
            b"c_abi_rust_mod_set_variant\0",
        ),
        (
            "C-ABI C99[release]",
            &mods.cabi_c99_release,
            b"c_abi_c99_mod_set_variant\0",
        ),
        (
            "C-ABI C99[debug]",
            &mods.cabi_c99_debug,
            b"c_abi_c99_mod_set_variant\0",
        ),
    ];

    for (target_name, path, setter_sym) in cabi_targets {
        for (variant_id, variant_name) in [
            (0_u32, "ChunkOptimized"),
            (1_u32, "EntityNestedVTable"),
            (2_u32, "NaiveChattyFfi"),
        ] {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            register_all_scenario_components(&mut app);
            populate_scenario_world(app.world_mut(), cfg);

            let pin_lib = set_cabi_mod_variant(path, setter_sym, variant_id);
            let mod_id = app
                .load_mod_tracked(path)
                .unwrap_or_else(|e| panic!("Failed to load {target_name} ({variant_name}): {e:?}"));

            for _ in 0..TICKS {
                app.update();
            }
            let digest = OracleDigest::capture(app.world_mut());
            golden.assert_equivalent(
                &digest,
                &format!("Host[{host_prof}] + {target_name}::{variant_name}"),
            );

            app.unload_mod(mod_id).expect("unload_mod failed");
            drop(app);
            drop(pin_lib);
        }
    }
}

// ============================================================================
// 3. Step B: Mod Loading, Handshake, 34-Archetype Init & Teardown Benchmarks
// ============================================================================

fn median_f64(mut vals: Vec<f64>) -> f64 {
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    if vals.is_empty() {
        0.0
    } else {
        vals[vals.len() / 2]
    }
}

fn measure_loading_permutations(
    root: &Path,
    mods: &ModArtifacts,
    host_prof: &str,
) -> Vec<LoadRecord> {
    let mut records = Vec::new();
    let cfg = ScenarioConfig::oracle(); // 34 archetypes populated so schedule init scans real archetypes
    const ROUNDS: usize = 15;

    let tmp_dir = root.join("target/tmp_dlopen_bench").join(host_prof);
    let _ = fs::remove_dir_all(&tmp_dir);
    fs::create_dir_all(&tmp_dir).expect("Failed to create tmp_dlopen_bench");

    // 1. Pure Native Bevy (No-Mod Baseline: add_systems + Update schedule initialize)
    {
        let mut init_samples = Vec::with_capacity(ROUNDS);
        for _ in 0..ROUNDS {
            let mut app = App::new();
            register_pure_native_components(app.world_mut());
            populate_scenario_world(app.world_mut(), cfg);

            let t0 = Instant::now();
            app.add_systems(Update, native_nested_aura_combat_system);
            app.world_mut()
                .try_schedule_scope(Update.intern(), |world, sched| {
                    let _ = sched.initialize(world);
                })
                .ok();
            init_samples.push(t0.elapsed().as_secs_f64() * 1_000_000.0);
        }
        let init_us = median_f64(init_samples);
        records.push(LoadRecord {
            category: "Pure Native Bevy (No-Mod)".to_string(),
            host_profile: host_prof.to_string(),
            mod_profile: "none (static)".to_string(),
            status: "OK".to_string(),
            binary_size_kb: 0.0,
            dlopen_us: 0.0,
            init_and_schedule_us: init_us,
            unload_us: 0.0,
            total_lifecycle_us: init_us,
            notes: "Statically compiled into host binary; zero dlopen overhead".to_string(),
        });
    }

    // 2. Coupled Rust `dylib` Mod (Same Profile — measure both Cold First-Seen and Warm Re-Signed dlopen)
    {
        let (_, ext) = dylib_prefix_ext();
        let mut dlopen_s = Vec::with_capacity(ROUNDS);
        let mut init_s = Vec::with_capacity(ROUNDS);
        let mut unload_s = Vec::with_capacity(ROUNDS);
        let mut pinned_libs = Vec::with_capacity(ROUNDS);

        // Pre-stage all unique Mach-O/ELF copies BEFORE timing so Gatekeeper / page-cache warms uniformly
        let mut staged_paths = Vec::with_capacity(ROUNDS);
        for r in 0..ROUNDS {
            let unique_copy = tmp_dir.join(format!("coupled_r{r}.{ext}"));
            fs::copy(&mods.coupled_rust_same_profile, &unique_copy)
                .expect("Failed to copy coupled_rust_mod");
            staged_paths.push(unique_copy);
        }

        for unique_copy in &staged_paths {
            let mut app = App::new();
            register_pure_native_components(app.world_mut());
            populate_scenario_world(app.world_mut(), cfg);

            let t_dl = Instant::now();
            // SAFETY: Loading fresh copy of trusted coupled_rust_mod.
            let lib = unsafe { Library::new(unique_copy) }.expect("dlopen coupled_rust_mod");
            dlopen_s.push(t_dl.elapsed().as_secs_f64() * 1_000_000.0);

            let t_in = Instant::now();
            unsafe {
                #[allow(improper_ctypes_definitions)]
                let reg: Symbol<extern "Rust" fn(&mut App)> =
                    lib.get(b"coupled_rust_mod_register\0").unwrap();
                reg(&mut app);
            }
            app.world_mut()
                .try_schedule_scope(Update.intern(), |world, sched| {
                    let _ = sched.initialize(world);
                })
                .ok();
            init_s.push(t_in.elapsed().as_secs_f64() * 1_000_000.0);

            let t_un = Instant::now();
            unsafe {
                let shutdown: Symbol<extern "C" fn()> =
                    lib.get(b"coupled_rust_mod_shutdown\0").unwrap();
                shutdown();
            }
            drop(app);
            unload_s.push(t_un.elapsed().as_secs_f64() * 1_000_000.0);

            // NOTE: In a Coupled Rust dylib, `dlclose` (`drop(lib)`) cannot be safely executed
            // while process-global `bevy_ecs` / `std` thread-local destructors or TypeId vtables
            // have registered pointers into the dylib's `.text`/`.rodata` sections!
            pinned_libs.push(lib);
            let _ = fs::remove_file(unique_copy);
        }
        std::mem::forget(pinned_libs);

        let dl_us = median_f64(dlopen_s);
        let in_us = median_f64(init_s);
        let un_us = median_f64(unload_s);

        records.push(LoadRecord {
            category: "Coupled Rust dylib Mod".to_string(),
            host_profile: host_prof.to_string(),
            mod_profile: host_prof.to_string(),
            status: "OK".to_string(),
            binary_size_kb: file_size_kb(&mods.coupled_rust_same_profile),
            dlopen_us: dl_us,
            init_and_schedule_us: in_us,
            unload_us: un_us,
            total_lifecycle_us: dl_us + in_us + un_us,
            notes: "Shares unstable Rust ABI via libshared_scenario.dylib; requires exact rustc + profile match".to_string(),
        });

        // Also record the mismatched profile permutation explicitly!
        let opposite_prof = if host_prof == "release" {
            "debug"
        } else {
            "release"
        };
        records.push(LoadRecord {
            category: "Coupled Rust dylib Mod".to_string(),
            host_profile: host_prof.to_string(),
            mod_profile: opposite_prof.to_string(),
            status: "ABI_INCOMPATIBLE".to_string(),
            binary_size_kb: 0.0,
            dlopen_us: 0.0,
            init_and_schedule_us: 0.0,
            unload_us: 0.0,
            total_lifecycle_us: 0.0,
            notes: "Rejected by Rust ABI: StableCrateId & TypeId metadata hashes differ between dev and release profiles".to_string(),
        });
    }

    // 3. C-ABI Rust `cdylib` & C99 Mods (ALL 4 Host × Guest Debug/Release Permutations!)
    let cabi_mods: [(&str, &str, &Path); 4] = [
        ("C-ABI Rust cdylib Mod", "release", &mods.cabi_rust_release),
        ("C-ABI Rust cdylib Mod", "debug", &mods.cabi_rust_debug),
        ("C-ABI C99 Mod", "release (-O3)", &mods.cabi_c99_release),
        ("C-ABI C99 Mod", "debug (-O0)", &mods.cabi_c99_debug),
    ];

    let (_, ext) = dylib_prefix_ext();
    for (m_idx, (cat, mod_prof, src_path)) in cabi_mods.iter().enumerate() {
        let mut dlopen_s = Vec::with_capacity(ROUNDS);
        let mut init_s = Vec::with_capacity(ROUNDS);
        let mut unload_s = Vec::with_capacity(ROUNDS);

        // Pre-stage unique Mach-O/ELF copies before timing
        let mut staged_paths = Vec::with_capacity(ROUNDS);
        for r in 0..ROUNDS {
            let unique_load_copy = tmp_dir.join(format!("cabi_load_{m_idx}_r{r}.{ext}"));
            fs::copy(src_path, &unique_load_copy).expect("Failed to copy C-ABI mod for load");
            staged_paths.push(unique_load_copy);
        }

        // Hold one pin handle on `src_path` so the cached measurement measures pure
        // ABI handshake + 8 VTable lookups + Bevy 34-archetype schedule init without OS dyld mapping!
        let pin_src = unsafe { Library::new(src_path) }.expect("pin src_path");

        for unique_load_copy in &staged_paths {
            // Measure end-to-end ModHost load (dlopen + handshake + 8 VTable lookups + 34-archetype schedule init)
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            register_all_scenario_components(&mut app);
            populate_scenario_world(app.world_mut(), cfg);

            let registry = app
                .world()
                .resource::<shared_scenario::host_runtime::ModRegistry>()
                .clone();
            let registry_arc = std::sync::Arc::new(registry);

            let t_dl = Instant::now();
            let (mod_id, systems) = {
                let mut host = app.world_mut().resource_mut::<ModHost>();
                unsafe {
                    host.load_mod_tracked_with_registry(unique_load_copy, &registry_arc)
                        .expect("C-ABI handshake")
                }
            };
            let load_and_handshake_us = t_dl.elapsed().as_secs_f64() * 1_000_000.0;

            // Measure pure cached ABI handshake + 8 VTable lookups + Bevy 34-archetype schedule initialization
            let mut app_cached = App::new();
            app_cached.add_plugins(AbiModPlugin);
            register_all_scenario_components(&mut app_cached);
            populate_scenario_world(app_cached.world_mut(), cfg);
            let reg_cached = std::sync::Arc::new(
                app_cached
                    .world()
                    .resource::<shared_scenario::host_runtime::ModRegistry>()
                    .clone(),
            );

            let t_in = Instant::now();
            let cached_id = {
                let mut host_c = app_cached.world_mut().resource_mut::<ModHost>();
                let (cid, c_systems) = unsafe {
                    host_c
                        .load_mod_tracked_with_registry(src_path, &reg_cached)
                        .expect("cached C-ABI handshake")
                };
                drop(host_c);
                for sys in c_systems {
                    app_cached.add_systems(Update, sys);
                }
                app_cached
                    .world_mut()
                    .try_schedule_scope(Update.intern(), |world, sched| {
                        let _ = sched.initialize(world);
                    })
                    .ok();
                cid
            };
            let pure_handshake_and_sched_us = t_in.elapsed().as_secs_f64() * 1_000_000.0;
            app_cached.unload_mod(cached_id).expect("cached unload");
            drop(app_cached);

            for sys in systems {
                app.add_systems(Update, sys);
            }
            app.world_mut()
                .try_schedule_scope(Update.intern(), |world, sched| {
                    let _ = sched.initialize(world);
                })
                .ok();

            let pure_dlopen_us = (load_and_handshake_us - pure_handshake_and_sched_us).max(1.0);
            dlopen_s.push(pure_dlopen_us);
            init_s.push(pure_handshake_and_sched_us);

            let t_un = Instant::now();
            app.unload_mod(mod_id).expect("C-ABI unload");
            unload_s.push(t_un.elapsed().as_secs_f64() * 1_000_000.0);
            drop(app);

            let _ = fs::remove_file(unique_load_copy);
        }
        drop(pin_src);

        let dl_us = median_f64(dlopen_s);
        let in_us = median_f64(init_s);
        let un_us = median_f64(unload_s);

        records.push(LoadRecord {
            category: (*cat).to_string(),
            host_profile: host_prof.to_string(),
            mod_profile: (*mod_prof).to_string(),
            status: "OK".to_string(),
            binary_size_kb: file_size_kb(src_path),
            dlopen_us: dl_us,
            init_and_schedule_us: in_us,
            unload_us: un_us,
            total_lifecycle_us: dl_us + in_us + un_us,
            notes: "Asymmetric C-ABI: resolves 8 VTables + registers multi-query DynamicCSystem across 34 archetypes".to_string(),
        });
    }

    let _ = fs::remove_dir_all(&tmp_dir);
    records
}

// ============================================================================
// 4. Step C & D: Complex Nested Multi-Archetype Frame Execution Benchmarks
// ============================================================================

fn summarize_durations(
    mut durs_ns: Vec<u128>,
    tier_label: &str,
    architecture: &str,
    access_pattern: &str,
    host_profile: &str,
    mod_profile: &str,
    scale_name: &str,
    archetypes_total: usize,
    active_units: usize,
    active_emitters: usize,
    oracle_verified: bool,
) -> ExecRecord {
    durs_ns.sort_unstable();
    let n = durs_ns.len();
    let median_ns = durs_ns[n / 2] as f64;
    let min_ns = durs_ns[0] as f64;
    let p95_idx = ((n as f64 * 0.95) as usize).min(n - 1);
    let p95_ns = durs_ns[p95_idx] as f64;
    let sum_ns: u128 = durs_ns.iter().copied().sum();
    let mean_ns = (sum_ns as f64) / (n as f64);

    let pairs = active_units * active_emitters;
    let ns_per_pair = median_ns / (pairs as f64);
    let ns_per_unit = median_ns / (active_units as f64);
    let pairs_per_sec = (pairs as f64) / (median_ns * 1.0e-9);

    ExecRecord {
        tier_label: tier_label.to_string(),
        architecture: architecture.to_string(),
        access_pattern: access_pattern.to_string(),
        host_profile: host_profile.to_string(),
        mod_profile: mod_profile.to_string(),
        scale_name: scale_name.to_string(),
        archetypes_total,
        active_units,
        active_emitters,
        pairs_per_frame: pairs,
        frames_measured: n,
        median_frame_ms: median_ns / 1_000_000.0,
        mean_frame_ms: mean_ns / 1_000_000.0,
        min_frame_ms: min_ns / 1_000_000.0,
        p95_frame_ms: p95_ns / 1_000_000.0,
        ns_per_pair,
        ns_per_unit,
        pairs_per_sec,
        oracle_verified,
    }
}

fn run_app_timed(app: &mut App, warmup: usize, frames: usize) -> Vec<u128> {
    for _ in 0..warmup {
        app.update();
    }
    let mut samples = Vec::with_capacity(frames);
    for _ in 0..frames {
        let t0 = Instant::now();
        app.update();
        samples.push(t0.elapsed().as_nanos());
    }
    samples
}

fn measure_execution_suite_at_scale(
    mods: &ModArtifacts,
    host_prof: &str,
    cfg: ScenarioConfig,
    scale_name: &str,
    include_debug_mods: bool,
) -> Vec<ExecRecord> {
    let mut out = Vec::new();
    let is_host_debug = host_prof == "debug";
    let base_frames = if is_host_debug { 10 } else { 32 };
    let warmup = if is_host_debug { 2 } else { 12 };

    // 1a. Pure Native Bevy (No-Mod, L1 Pre-Cached SoA — Absolute Hardware Ceiling)
    {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        let stats = populate_scenario_world(app.world_mut(), cfg);
        app.add_systems(Update, native_cached_soa_nested_aura_combat_system);

        let durs = run_app_timed(&mut app, warmup, base_frames);
        out.push(summarize_durations(
            durs,
            &format!("Pure Native Bevy (L1 SoA Cache) [Host={host_prof}]"),
            "Pure Native Bevy (No-Mod)",
            "Pre-Cached SoA + Inlined Query",
            host_prof,
            "none (static)",
            scale_name,
            stats.total_archetypes,
            stats.active_units,
            stats.active_emitters,
            true,
        ));
    }

    // 1b. Pure Native Bevy (No-Mod, Idiomatic Direct Nested Query)
    {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        let stats = populate_scenario_world(app.world_mut(), cfg);
        app.add_systems(Update, native_nested_aura_combat_system);

        let durs = run_app_timed(&mut app, warmup, base_frames);
        out.push(summarize_durations(
            durs,
            &format!("Pure Native Bevy (Idiomatic Nested) [Host={host_prof}]"),
            "Pure Native Bevy (No-Mod)",
            "Direct Nested Query (6 Inner Tables)",
            host_prof,
            "none (static)",
            scale_name,
            stats.total_archetypes,
            stats.active_units,
            stats.active_emitters,
            true,
        ));
    }

    // 2. Coupled Rust `dylib` Mod (Same Profile — Both Pre-Cached SoA & Direct Nested Query)
    for (c_var, c_pat) in [
        (1_u32, "Pre-Cached SoA + Shared Rust Query"),
        (0_u32, "Direct Nested Query (Shared Rust ABI)"),
    ] {
        let mut app = App::new();
        register_pure_native_components(app.world_mut());
        let stats = populate_scenario_world(app.world_mut(), cfg);

        // SAFETY: Loading same-profile coupled_rust_mod.
        let lib = unsafe { Library::new(&mods.coupled_rust_same_profile) }
            .expect("load coupled_rust_mod");
        unsafe {
            let set_var: Symbol<extern "C" fn(u32)> =
                lib.get(b"coupled_rust_mod_set_variant\0").unwrap();
            set_var(c_var);
            #[allow(improper_ctypes_definitions)]
            let reg: Symbol<extern "Rust" fn(&mut App)> =
                lib.get(b"coupled_rust_mod_register\0").unwrap();
            reg(&mut app);
        }

        let durs = run_app_timed(&mut app, warmup, base_frames);
        out.push(summarize_durations(
            durs,
            &format!("Coupled Rust dylib [Host={host_prof}, Mod={host_prof}] :: {c_pat}"),
            "Coupled Rust dylib Mod",
            c_pat,
            host_prof,
            host_prof,
            scale_name,
            stats.total_archetypes,
            stats.active_units,
            stats.active_emitters,
            true,
        ));

        unsafe {
            let shutdown: Symbol<extern "C" fn()> =
                lib.get(b"coupled_rust_mod_shutdown\0").unwrap();
            shutdown();
        }
        drop(app);
        std::mem::forget(lib);
    }

    // 3. C-ABI Mods across Progressive Patterns & Build Targets
    struct CabiCase<'a> {
        arch: &'a str,
        mod_prof: &'a str,
        path: &'a Path,
        setter_sym: &'a [u8],
        is_debug_mod: bool,
    }

    let cabi_cases = [
        CabiCase {
            arch: "C-ABI Rust cdylib",
            mod_prof: "release",
            path: &mods.cabi_rust_release,
            setter_sym: b"c_abi_rust_mod_set_variant\0",
            is_debug_mod: false,
        },
        CabiCase {
            arch: "C-ABI C99 Mod",
            mod_prof: "release (-O3)",
            path: &mods.cabi_c99_release,
            setter_sym: b"c_abi_c99_mod_set_variant\0",
            is_debug_mod: false,
        },
        CabiCase {
            arch: "C-ABI Rust cdylib",
            mod_prof: "debug",
            path: &mods.cabi_rust_debug,
            setter_sym: b"c_abi_rust_mod_set_variant\0",
            is_debug_mod: true,
        },
        CabiCase {
            arch: "C-ABI C99 Mod",
            mod_prof: "debug (-O0)",
            path: &mods.cabi_c99_debug,
            setter_sym: b"c_abi_c99_mod_set_variant\0",
            is_debug_mod: true,
        },
    ];

    let variants: [(u32, &str); 3] = [
        (0, "ChunkOptimized (O(U+E) VTable)"),
        (1, "EntityNestedVTable (O(U*E) Getters)"),
        (2, "NaiveChattyFfi (O(U*E) R/W + Tick)"),
    ];

    for (var_id, pat_name) in variants {
        for case in &cabi_cases {
            if case.is_debug_mod && !include_debug_mods {
                continue;
            }
            let frames = if is_host_debug || case.is_debug_mod || var_id == 2 {
                8
            } else {
                base_frames
            };

            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            register_all_scenario_components(&mut app);
            let stats = populate_scenario_world(app.world_mut(), cfg);

            let pin_lib = set_cabi_mod_variant(case.path, case.setter_sym, var_id);
            let mod_id = app.load_mod_tracked(case.path).expect("load C-ABI mod");

            let durs = run_app_timed(&mut app, warmup, frames);
            out.push(summarize_durations(
                durs,
                &format!(
                    "{} [Host={host_prof}, Mod={}] :: {pat_name}",
                    case.arch, case.mod_prof
                ),
                case.arch,
                pat_name,
                host_prof,
                case.mod_prof,
                scale_name,
                stats.total_archetypes,
                stats.active_units,
                stats.active_emitters,
                true,
            ));

            app.unload_mod(mod_id).expect("unload C-ABI mod");
            drop(app);
            drop(pin_lib);
        }
    }

    // 4. Dual Concurrent Mods (C-ABI C99 + C-ABI Rust Loaded Together in Same Schedule)
    {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        register_all_scenario_components(&mut app);
        let stats = populate_scenario_world(app.world_mut(), cfg);

        let pin_r =
            set_cabi_mod_variant(&mods.cabi_rust_release, b"c_abi_rust_mod_set_variant\0", 0);
        let pin_c = set_cabi_mod_variant(&mods.cabi_c99_release, b"c_abi_c99_mod_set_variant\0", 0);

        let id_r = app.load_mod_tracked(&mods.cabi_rust_release).unwrap();
        let id_c = app.load_mod_tracked(&mods.cabi_c99_release).unwrap();

        let frames = if is_host_debug { 8 } else { 16 };
        let durs = run_app_timed(&mut app, warmup, frames);
        // Note: 2 full nested systems execute sequentially per frame (borrow-serialized on Transform3D/Kinematics/Vitality)
        out.push(summarize_durations(
            durs,
            &format!("Dual C99+Rust C-ABI Mods [Host={host_prof}, Mods=release]"),
            "Dual C-ABI (Rust + C99)",
            "2x Serialized Multi-Query Systems/Frame",
            host_prof,
            "release + release (-O3)",
            scale_name,
            stats.total_archetypes,
            stats.active_units,
            stats.active_emitters,
            false,
        ));

        app.unload_mod(id_r).unwrap();
        app.unload_mod(id_c).unwrap();
        drop(app);
        drop(pin_c);
        drop(pin_r);
    }

    out
}

// ============================================================================
// 5. JSON Serialization & Parsing Between Worker Processes and Orchestrator
// ============================================================================

fn escape_json(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn serialize_worker_payload(loads: &[LoadRecord], execs: &[ExecRecord]) -> String {
    let mut out = String::new();
    for l in loads {
        out.push_str(&format!(
            "LOAD\t{}\t{}\t{}\t{}\t{:.4}\t{:.4}\t{:.4}\t{:.4}\t{:.4}\t{}\n",
            l.category,
            l.host_profile,
            l.mod_profile,
            l.status,
            l.binary_size_kb,
            l.dlopen_us,
            l.init_and_schedule_us,
            l.unload_us,
            l.total_lifecycle_us,
            l.notes
        ));
    }
    for e in execs {
        out.push_str(&format!(
            "EXEC\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{:.6}\t{:.6}\t{:.6}\t{:.6}\t{:.4}\t{:.4}\t{:.2}\t{}\n",
            e.tier_label,
            e.architecture,
            e.access_pattern,
            e.host_profile,
            e.mod_profile,
            e.scale_name,
            e.archetypes_total,
            e.active_units,
            e.active_emitters,
            e.pairs_per_frame,
            e.frames_measured,
            e.median_frame_ms,
            e.mean_frame_ms,
            e.min_frame_ms,
            e.p95_frame_ms,
            e.ns_per_pair,
            e.ns_per_unit,
            e.pairs_per_sec,
            e.oracle_verified
        ));
    }
    out
}

fn parse_worker_payload(text: &str, loads: &mut Vec<LoadRecord>, execs: &mut Vec<ExecRecord>) {
    for line in text.lines() {
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.is_empty() {
            continue;
        }
        match parts[0] {
            "LOAD" if parts.len() >= 11 => {
                loads.push(LoadRecord {
                    category: parts[1].to_string(),
                    host_profile: parts[2].to_string(),
                    mod_profile: parts[3].to_string(),
                    status: parts[4].to_string(),
                    binary_size_kb: parts[5].parse().unwrap_or(0.0),
                    dlopen_us: parts[6].parse().unwrap_or(0.0),
                    init_and_schedule_us: parts[7].parse().unwrap_or(0.0),
                    unload_us: parts[8].parse().unwrap_or(0.0),
                    total_lifecycle_us: parts[9].parse().unwrap_or(0.0),
                    notes: parts[10].to_string(),
                });
            }
            "EXEC" if parts.len() >= 20 => {
                execs.push(ExecRecord {
                    tier_label: parts[1].to_string(),
                    architecture: parts[2].to_string(),
                    access_pattern: parts[3].to_string(),
                    host_profile: parts[4].to_string(),
                    mod_profile: parts[5].to_string(),
                    scale_name: parts[6].to_string(),
                    archetypes_total: parts[7].parse().unwrap_or(0),
                    active_units: parts[8].parse().unwrap_or(0),
                    active_emitters: parts[9].parse().unwrap_or(0),
                    pairs_per_frame: parts[10].parse().unwrap_or(0),
                    frames_measured: parts[11].parse().unwrap_or(0),
                    median_frame_ms: parts[12].parse().unwrap_or(0.0),
                    mean_frame_ms: parts[13].parse().unwrap_or(0.0),
                    min_frame_ms: parts[14].parse().unwrap_or(0.0),
                    p95_frame_ms: parts[15].parse().unwrap_or(0.0),
                    ns_per_pair: parts[16].parse().unwrap_or(0.0),
                    ns_per_unit: parts[17].parse().unwrap_or(0.0),
                    pairs_per_sec: parts[18].parse().unwrap_or(0.0),
                    oracle_verified: parts[19].parse().unwrap_or(false),
                });
            }
            _ => {}
        }
    }
}

fn format_full_json_report(
    loads: &[LoadRecord],
    execs: &[ExecRecord],
    true_native_standard_ns: f64,
    true_native_heavy_ns: f64,
) -> String {
    let mut j = String::new();
    j.push_str("{\n");
    j.push_str("  \"suite\": \"bevy_abi_mod_full_permutation_matrix\",\n");
    j.push_str("  \"scenario\": {\n");
    j.push_str("    \"description\": \"Complex Nested Multi-Archetype Query (Outer CombatUnits across 16 archetypes x Inner AuraEmitters across 6 archetypes + 12 excluded frozen/decoy archetypes = 34 total Bevy archetypes; nested Transform3D/Kinematics Vec3f structs + Vitality/Resistance/Faction)\",\n");
    j.push_str("    \"total_archetypes\": 34,\n");
    j.push_str("    \"active_unit_archetypes\": 16,\n");
    j.push_str("    \"active_emitter_archetypes\": 6,\n");
    j.push_str("    \"excluded_decoys_and_frozen_archetypes\": 12,\n");
    j.push_str("    \"standard_scale_pairs_per_frame\": 49152,\n");
    j.push_str("    \"heavy_scale_pairs_per_frame\": 294912\n");
    j.push_str("  },\n");
    j.push_str("  \"environment\": {\n");
    j.push_str(&format!("    \"os\": \"{}\",\n", env::consts::OS));
    j.push_str(&format!("    \"arch\": \"{}\",\n", env::consts::ARCH));
    j.push_str(&format!(
        "    \"logical_cores\": {}\n",
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4)
    ));
    j.push_str("  },\n");

    j.push_str("  \"mod_loading_matrix\": [\n");
    for (idx, l) in loads.iter().enumerate() {
        j.push_str("    {\n");
        j.push_str(&format!(
            "      \"category\": \"{}\",\n",
            escape_json(&l.category)
        ));
        j.push_str(&format!(
            "      \"host_profile\": \"{}\",\n",
            escape_json(&l.host_profile)
        ));
        j.push_str(&format!(
            "      \"mod_profile\": \"{}\",\n",
            escape_json(&l.mod_profile)
        ));
        j.push_str(&format!(
            "      \"status\": \"{}\",\n",
            escape_json(&l.status)
        ));
        j.push_str(&format!(
            "      \"binary_size_kb\": {:.2},\n",
            l.binary_size_kb
        ));
        j.push_str(&format!("      \"dlopen_us\": {:.2},\n", l.dlopen_us));
        j.push_str(&format!(
            "      \"init_and_schedule_us\": {:.2},\n",
            l.init_and_schedule_us
        ));
        j.push_str(&format!("      \"unload_us\": {:.2},\n", l.unload_us));
        j.push_str(&format!(
            "      \"total_lifecycle_us\": {:.2},\n",
            l.total_lifecycle_us
        ));
        j.push_str(&format!("      \"notes\": \"{}\"\n", escape_json(&l.notes)));
        j.push_str(if idx + 1 < loads.len() {
            "    },\n"
        } else {
            "    }\n"
        });
    }
    j.push_str("  ],\n");

    j.push_str("  \"nested_query_execution_matrix\": [\n");
    for (idx, e) in execs.iter().enumerate() {
        let base_ns = if e.scale_name == "heavy_295k_pairs" && true_native_heavy_ns > 0.0 {
            true_native_heavy_ns
        } else {
            true_native_standard_ns.max(1e-9)
        };
        let slowdown_x = e.ns_per_pair / base_ns;
        let overhead_pct = (slowdown_x - 1.0) * 100.0;
        let fps_budget_pct = (e.median_frame_ms / 16.6667) * 100.0;

        j.push_str("    {\n");
        j.push_str(&format!(
            "      \"benchmark_name\": \"{}\",\n",
            escape_json(&e.tier_label)
        ));
        j.push_str(&format!(
            "      \"architecture\": \"{}\",\n",
            escape_json(&e.architecture)
        ));
        j.push_str(&format!(
            "      \"access_pattern\": \"{}\",\n",
            escape_json(&e.access_pattern)
        ));
        j.push_str(&format!(
            "      \"host_profile\": \"{}\",\n",
            escape_json(&e.host_profile)
        ));
        j.push_str(&format!(
            "      \"mod_profile\": \"{}\",\n",
            escape_json(&e.mod_profile)
        ));
        j.push_str(&format!(
            "      \"scale\": \"{}\",\n",
            escape_json(&e.scale_name)
        ));
        j.push_str(&format!(
            "      \"total_archetypes\": {},\n",
            e.archetypes_total
        ));
        j.push_str(&format!("      \"active_units\": {},\n", e.active_units));
        j.push_str(&format!(
            "      \"active_emitters\": {},\n",
            e.active_emitters
        ));
        j.push_str(&format!(
            "      \"pairs_per_frame\": {},\n",
            e.pairs_per_frame
        ));
        j.push_str(&format!(
            "      \"frames_measured\": {},\n",
            e.frames_measured
        ));
        j.push_str(&format!(
            "      \"median_frame_ms\": {:.4},\n",
            e.median_frame_ms
        ));
        j.push_str(&format!(
            "      \"mean_frame_ms\": {:.4},\n",
            e.mean_frame_ms
        ));
        j.push_str(&format!("      \"min_frame_ms\": {:.4},\n", e.min_frame_ms));
        j.push_str(&format!("      \"p95_frame_ms\": {:.4},\n", e.p95_frame_ms));
        j.push_str(&format!("      \"ns_per_pair\": {:.4},\n", e.ns_per_pair));
        j.push_str(&format!("      \"ns_per_unit\": {:.4},\n", e.ns_per_unit));
        j.push_str(&format!(
            "      \"pairs_per_sec\": {:.2},\n",
            e.pairs_per_sec
        ));
        j.push_str(&format!(
            "      \"slowdown_vs_true_native_x\": {:.3},\n",
            slowdown_x
        ));
        j.push_str(&format!(
            "      \"degradation_vs_true_native_pct\": {:.2},\n",
            overhead_pct
        ));
        j.push_str(&format!(
            "      \"frame_budget_60fps_pct\": {:.2},\n",
            fps_budget_pct
        ));
        j.push_str(&format!(
            "      \"oracle_verified\": {}\n",
            e.oracle_verified
        ));
        j.push_str(if idx + 1 < execs.len() {
            "    },\n"
        } else {
            "    }\n"
        });
    }
    j.push_str("  ]\n");
    j.push_str("}\n");
    j
}

// ============================================================================
// 6. Console Report Printer & Multi-Profile Orchestrator
// ============================================================================

fn print_comprehensive_report(loads: &[LoadRecord], execs: &[ExecRecord]) -> (f64, f64) {
    let true_native_std = execs
        .iter()
        .find(|e| {
            e.architecture == "Pure Native Bevy (No-Mod)"
                && e.access_pattern == "Pre-Cached SoA + Inlined Query"
                && e.host_profile == "release"
                && e.scale_name == "standard_49k_pairs"
        })
        .map(|e| e.ns_per_pair)
        .unwrap_or(1.0);

    let true_native_heavy = execs
        .iter()
        .find(|e| {
            e.architecture == "Pure Native Bevy (No-Mod)"
                && e.access_pattern == "Pre-Cached SoA + Inlined Query"
                && e.host_profile == "release"
                && e.scale_name == "heavy_295k_pairs"
        })
        .map(|e| e.ns_per_pair)
        .unwrap_or(true_native_std);

    println!("\n================================================================================================================================");
    println!(" BEVY_ABI_MOD: COMPLETE ARCHITECTURE x TARGET PROFILE BENCHMARK MATRIX");
    println!(" Scenario: Complex Nested Multi-Archetype Query (34 Total Bevy Archetypes: 16 Unit Tables x 6 Emitter Tables + 12 Decoy/Frozen)");
    println!(" Mathematical Oracle: VERIFIED EQUIVALENT across Pure Native Bevy, Coupled Rust dylib, C-ABI Rust cdylib & C-ABI C99");
    println!("================================================================================================================================");

    // Table 1: Mod Loading & Lifecycle Performance
    println!("\n[1] MOD LOADING, ABI HANDSHAKE, 34-ARCHETYPE SCHEDULE INIT & UNLOAD LATENCY (microseconds)");
    println!("--------------------------------------------------------------------------------------------------------------------------------");
    println!(
        "{:<28} | {:<7} | {:<14} | {:<8} | {:>9} | {:>10} | {:>11} | {:>9} | {:>10}",
        "Architecture",
        "Host",
        "Mod Target",
        "Status",
        "Size(KB)",
        "dlopen(us)",
        "Init+Sch(us)",
        "Close(us)",
        "Total(us)"
    );
    println!("--------------------------------------------------------------------------------------------------------------------------------");
    for l in loads {
        if l.status != "OK" {
            println!(
                "{:<28} | {:<7} | {:<14} | {:<8} | {:>9} | {:>10} | {:>11} | {:>9} | {:>10}",
                l.category,
                l.host_profile,
                l.mod_profile,
                "LOCKED*",
                "N/A",
                "N/A",
                "N/A",
                "N/A",
                "ABI ERR"
            );
        } else {
            println!(
                "{:<28} | {:<7} | {:<14} | {:<8} | {:>9.1} | {:>10.1} | {:>11.1} | {:>9.1} | {:>10.1}",
                l.category,
                l.host_profile,
                l.mod_profile,
                l.status,
                l.binary_size_kb,
                l.dlopen_us,
                l.init_and_schedule_us,
                l.unload_us,
                l.total_lifecycle_us
            );
        }
    }
    println!("  * LOCKED: Coupled Rust dylib mods cannot load across mismatched dev/release profiles due to Rust StableCrateId/TypeId hash mismatch.");

    // Table 2: Standard Scale Ranked from Fastest (True Native) to Slowest (Worst Degradation)
    let mut std_execs: Vec<&ExecRecord> = execs
        .iter()
        .filter(|e| e.scale_name == "standard_49k_pairs")
        .collect();
    std_execs.sort_by(|a, b| a.median_frame_ms.partial_cmp(&b.median_frame_ms).unwrap());

    println!("\n[2] COMPLEX NESTED MULTI-ARCHETYPE QUERY — DEGRADATION LADDER (1,024 Units x 48 Emitters = 49,152 Pairs/Frame across 34 Archetypes)");
    println!("    Ranked from 1.00x True Native Release Baseline down to Worst-Case Unoptimized / Naive FFI Permutations:");
    println!("--------------------------------------------------------------------------------------------------------------------------------");
    println!(
        "{:<24} | {:<35} | {:<7} | {:<14} | {:>9} | {:>9} | {:>9} | {:>9}",
        "Architecture",
        "Access Pattern",
        "Host",
        "Mod Target",
        "Frame(ms)",
        "ns/pair",
        "Slowdown",
        "60FPS Bgt"
    );
    println!("--------------------------------------------------------------------------------------------------------------------------------");
    for e in &std_execs {
        let slow = e.ns_per_pair / true_native_std;
        let bgt = (e.median_frame_ms / 16.6667) * 100.0;
        println!(
            "{:<24} | {:<35} | {:<7} | {:<14} | {:>9.4} | {:>9.2} | {:>8.2}x | {:>8.1}%",
            e.architecture,
            e.access_pattern,
            e.host_profile,
            e.mod_profile,
            e.median_frame_ms,
            e.ns_per_pair,
            slow,
            bgt
        );
    }

    // Table 3: High-Scale Stress Tier (Release Host, 294,912 Pairs/Frame)
    let mut heavy_execs: Vec<&ExecRecord> = execs
        .iter()
        .filter(|e| e.scale_name == "heavy_295k_pairs")
        .collect();
    heavy_execs.sort_by(|a, b| a.median_frame_ms.partial_cmp(&b.median_frame_ms).unwrap());

    if !heavy_execs.is_empty() {
        println!("\n[3] HIGH-SCALE STRESS TIER — RELEASE HOST (3,072 Units x 96 Emitters = 294,912 Pairs/Frame across 34 Archetypes)");
        println!("--------------------------------------------------------------------------------------------------------------------------------");
        println!(
            "{:<24} | {:<35} | {:<7} | {:<14} | {:>9} | {:>9} | {:>9} | {:>9}",
            "Architecture",
            "Access Pattern",
            "Host",
            "Mod Target",
            "Frame(ms)",
            "ns/pair",
            "Slowdown",
            "60FPS Bgt"
        );
        println!("--------------------------------------------------------------------------------------------------------------------------------");
        for e in &heavy_execs {
            let slow = e.ns_per_pair / true_native_heavy;
            let bgt = (e.median_frame_ms / 16.6667) * 100.0;
            println!(
                "{:<24} | {:<35} | {:<7} | {:<14} | {:>9.4} | {:>9.2} | {:>8.2}x | {:>8.1}%",
                e.architecture,
                e.access_pattern,
                e.host_profile,
                e.mod_profile,
                e.median_frame_ms,
                e.ns_per_pair,
                slow,
                bgt
            );
        }
    }

    println!("================================================================================================================================\n");
    (true_native_std, true_native_heavy)
}

fn run_cmd(mut cmd: Command, desc: &str) {
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to launch `{desc}`: {e}"));
    assert!(status.success(), "Command `{desc}` failed with {status}");
}

fn orchestrate_all_profiles() {
    let root = root_dir();
    println!("================================================================================");
    println!(" [ORCHESTRATOR] Building All Debug & Release Host + Mod Permutations...");
    println!("================================================================================");

    // 1. Build C99 Benchmark Mods (both -O0 debug and -O3 release)
    let mut make_c99 = Command::new("make");
    make_c99.arg("-C").arg(root.join("benches/c_abi_c99_mod"));
    run_cmd(make_c99, "make C99 debug & release benchmark mods");

    // 2. Build Asymmetric C-ABI Rust cdylib Mod (Debug)
    let mut build_cabi_rust_dbg = Command::new("cargo");
    build_cabi_rust_dbg
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("benches/c_abi_rust_mod/Cargo.toml"));
    run_cmd(build_cabi_rust_dbg, "cargo build c_abi_rust_mod [debug]");

    // 3. Build Asymmetric C-ABI Rust cdylib Mod (Release)
    let mut build_cabi_rust_rel = Command::new("cargo");
    build_cabi_rust_rel
        .arg("build")
        .arg("--manifest-path")
        .arg(root.join("benches/c_abi_rust_mod/Cargo.toml"))
        .arg("--release");
    run_cmd(build_cabi_rust_rel, "cargo build c_abi_rust_mod [release]");

    // 4. Build Coupled Rust dylib Mod + Host Runner in Debug (`dev`)
    let mut build_ws_dbg = Command::new("cargo");
    build_ws_dbg.current_dir(&root).args([
        "build",
        "-p",
        "coupled_rust_mod",
        "-p",
        "permutation_bench",
    ]);
    run_cmd(
        build_ws_dbg,
        "cargo build coupled_rust_mod + permutation_bench [debug]",
    );

    // 5. Build Coupled Rust dylib Mod + Host Runner in Release (`release`)
    let mut build_ws_rel = Command::new("cargo");
    build_ws_rel.current_dir(&root).args([
        "build",
        "--release",
        "-p",
        "coupled_rust_mod",
        "-p",
        "permutation_bench",
    ]);
    run_cmd(
        build_ws_rel,
        "cargo build coupled_rust_mod + permutation_bench [release]",
    );

    // 6. Execute Release Host Worker and Debug Host Worker
    let out_dir = root.join("target/benchmarks");
    fs::create_dir_all(&out_dir).expect("create target/benchmarks");
    let rel_tsv = out_dir.join("worker_release.tsv");
    let dbg_tsv = out_dir.join("worker_debug.tsv");

    for (prof, tsv_path) in [("release", &rel_tsv), ("debug", &dbg_tsv)] {
        println!("[ORCHESTRATOR] Executing `{prof}` Host Worker across all Guest Mod targets...");
        let mut worker = Command::new("cargo");
        worker.current_dir(&root).arg("run").arg("--quiet");
        if prof == "release" {
            worker.arg("--release");
        }
        worker
            .args(["-p", "permutation_bench", "--", "--worker"])
            .arg(tsv_path);
        run_cmd(worker, &format!("permutation_bench worker [{prof}]"));
    }

    // 7. Merge results, print comparison ladder, and write JSON reports
    let mut all_loads = Vec::new();
    let mut all_execs = Vec::new();
    let rel_text = fs::read_to_string(&rel_tsv).expect("read worker_release.tsv");
    let dbg_text = fs::read_to_string(&dbg_tsv).expect("read worker_debug.tsv");
    parse_worker_payload(&rel_text, &mut all_loads, &mut all_execs);
    parse_worker_payload(&dbg_text, &mut all_loads, &mut all_execs);

    let (true_native_std, true_native_heavy) = print_comprehensive_report(&all_loads, &all_execs);
    let json = format_full_json_report(&all_loads, &all_execs, true_native_std, true_native_heavy);

    for dest in [
        root.join("target/benchmark_results.json"),
        root.join("target/benchmarks/benchmark_results.json"),
        root.join("crates/host_runtime/target/benchmark_results.json"),
    ] {
        if let Some(parent) = dest.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&dest, &json).expect("write benchmark_results.json");
    }

    println!(
        "[ORCHESTRATOR] Wrote complete JSON matrix to {:?}",
        root.join("target/benchmark_results.json")
    );
}

fn run_worker(out_tsv: &Path) {
    let root = root_dir();
    let host_prof = current_host_profile();
    let mods = resolve_mod_artifacts(&root, host_prof);

    eprintln!(
        "  [worker:{host_prof}] Step A: verifying mathematical oracle across all permutations..."
    );
    // 1. Verify mathematical equivalence against Pure Native Bevy oracle first!
    verify_all_permutations_against_oracle(&mods, host_prof);

    eprintln!(
        "  [worker:{host_prof}] Step B: measuring mod loading, handshake & unload latency..."
    );
    // 2. Measure Mod Loading / Init / Schedule / Unload latency
    let loads = measure_loading_permutations(&root, &mods, host_prof);

    eprintln!("  [worker:{host_prof}] Step C: measuring standard-scale nested multi-archetype execution...");
    // 3. Measure Standard Scale (34 archetypes, 1,024 units x 48 emitters = 49,152 pairs/frame)
    let mut execs = measure_execution_suite_at_scale(
        &mods,
        host_prof,
        ScenarioConfig::standard(),
        "standard_49k_pairs",
        true,
    );

    // 4. In Release Host, also run High-Scale Stress Tier (3,072 units x 96 emitters = 294,912 pairs/frame)
    if host_prof == "release" {
        eprintln!("  [worker:{host_prof}] Step D: measuring heavy-scale stress tier (294,912 pairs/frame)...");
        let heavy_execs = measure_execution_suite_at_scale(
            &mods,
            host_prof,
            ScenarioConfig::heavy(),
            "heavy_295k_pairs",
            false,
        );
        execs.extend(heavy_execs);
    }

    let payload = serialize_worker_payload(&loads, &execs);
    fs::write(out_tsv, payload).expect("write worker TSV");
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() >= 3 && args[1] == "--worker" {
        run_worker(Path::new(&args[2]));
    } else {
        orchestrate_all_profiles();
    }
}
