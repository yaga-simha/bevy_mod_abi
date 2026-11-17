//! Canonical ABI `0.2.0` Benchmark Suite.
//!
//! Unifies the five core `bevy_abi_mod` performance scenarios into a single zero-dependency,
//! Criterion-compatible benchmark harness (`--bench`, `--test`, `--quick`, and substring filter
//! arguments supported):
//!
//! 1. **`dense_iterate`** — Contiguous SoA chunk iteration at `1k`, `10k`, and `100k` entities
//!    comparing Native Bevy, GuestSystem C-ABI, Rust `.cdylib`, and ISO C99 `.dylib`/`.so`.
//! 2. **`changed_filter`** — `Changed<Position>` tick filtering over `100k` entities at `1%`,
//!    `50%`, and `100%` mutation rates (`table summary-tick skip + branch-light scan).
//! 3. **`command_burst`** — `10k` handle-resolved `host_cmd_insert` writes/frame into reusable
//!    per-system `CommandArena`.
//! 4. **`sparse_query`** — Batched indirect `StorageType::SparseSet` row-pointer iteration over
//!    `10k` entities.
//! 5. **`spawn_with_4`** — Batched single-archetype `host_cmd_spawn` with 4 components vs.
//!    sequential `host_cmd_spawn_empty` + 4x `host_cmd_insert`.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::cast_ptr_alignment, unreachable_pub)]

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_cmd_insert, host_cmd_spawn, host_cmd_spawn_empty,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_rows_mutable,
    host_iter_entities, host_iter_next_chunk, host_sys_commands, host_sys_query,
};
use bevy_abi_mod_sys::{
    AccessMode, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, FfiBool, FfiEntity,
    FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::Changed;
use bevy_ecs::system::{Commands, Query};
use core::ffi::c_void;
use std::env;
use std::hint::black_box;
use std::path::PathBuf;
use std::time::{Duration, Instant};

// ============================================================================
// Benchmark ECS Components
// ============================================================================

/// Dense 12-byte 3D position (`StorageType::Table`).
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

/// Dense 12-byte 3D velocity (`StorageType::Table`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "testbed::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    /// Z velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

/// Dense 8-byte vitality payload (`StorageType::Table`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bench::Vitality")]
pub struct Vitality {
    /// Health pool.
    pub hp: f32,
    /// Shield pool.
    pub shield: u32,
}

/// Dense 8-byte armor payload (`StorageType::Table`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "bench::Armor")]
pub struct Armor {
    /// Armor rating.
    pub plating: f32,
    /// Faction identifier.
    pub faction: u32,
}

/// Sparse-set 8-byte buff payload (`StorageType::SparseSet`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "bench::SparseBuff")]
pub struct SparseBuff {
    /// Multiplier applied during sparse iteration.
    pub bonus: f32,
    /// Remaining ticks.
    pub remaining: u32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[repr(C)]
struct RawRuntimePayload {
    primary: f32,
    secondary: u32,
}

// ============================================================================
// CLI Mode Parsing & Timing Harness
// ============================================================================

#[derive(Clone, Debug)]
struct BenchConfig {
    test_mode: bool,
    quick_mode: bool,
    warmup: usize,
    rounds: usize,
    filter: Option<String>,
}

impl BenchConfig {
    fn from_args() -> Self {
        let mut test_mode = false;
        let mut quick_mode = false;
        let mut filter: Option<String> = None;

        for arg in env::args().skip(1) {
            match arg.as_str() {
                "--test" => test_mode = true,
                "--quick" => quick_mode = true,
                "--bench" | "--nocapture" => {}
                other if !other.starts_with('-') => {
                    filter = Some(other.to_owned());
                }
                _ => {}
            }
        }

        let (warmup, rounds) = if test_mode {
            (1, 2)
        } else if quick_mode {
            (3, 12)
        } else {
            (5, 30)
        };

        Self {
            test_mode,
            quick_mode,
            warmup,
            rounds,
            filter,
        }
    }

    fn matches(&self, id: &str) -> bool {
        match &self.filter {
            Some(pat) => id.contains(pat.as_str()),
            None => true,
        }
    }

    fn scale_count(&self, full: usize) -> usize {
        if self.test_mode { full.min(256) } else { full }
    }
}

#[derive(Clone, Copy, Debug)]
struct Stats {
    median_ms: f64,
    p95_ms: f64,
    ops_per_sec: f64,
    ns_per_op: f64,
}

#[inline]
fn timed_update(app: &mut App) -> Duration {
    let start = Instant::now();
    app.update();
    start.elapsed()
}

fn measure_fn<F: FnMut() -> Duration>(
    mut f: F,
    warmup: usize,
    rounds: usize,
    ops_per_iter: usize,
) -> Stats {
    for _ in 0..warmup {
        let _ = f();
    }

    let mut samples = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        samples.push(f());
    }

    samples.sort();
    let median_ms = samples[rounds / 2].as_secs_f64() * 1000.0;
    let p95_idx = ((rounds * 95) / 100).min(rounds.saturating_sub(1));
    let p95_ms = samples[p95_idx].as_secs_f64() * 1000.0;
    let total: Duration = samples.iter().sum();
    let avg_secs = total.as_secs_f64() / (rounds as f64);
    let ops = ops_per_iter.max(1) as f64;
    let ops_per_sec = ops / avg_secs.max(1e-12);
    let ns_per_op = (median_ms * 1_000_000.0) / ops;

    Stats {
        median_ms,
        p95_ms,
        ops_per_sec,
        ns_per_op,
    }
}

fn print_row(label: &str, stats: Stats, unit: &str) {
    println!(
        "  {:<42} | median {:>8.4} ms | p95 {:>8.4} ms | {:>13.0} {unit}/s | {:>6.2} ns/{unit}",
        label, stats.median_ms, stats.p95_ms, stats.ops_per_sec, stats.ns_per_op
    );
}

// ============================================================================
// Fixture Discovery
// ============================================================================

fn try_get_dylib_paths() -> Option<(PathBuf, PathBuf)> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root_dir = if manifest_dir.ends_with("bevy_abi_mod_host") {
        manifest_dir.parent()?.parent()?.to_path_buf()
    } else {
        manifest_dir
    };

    let fixture_dir = env::var_os("BAM_FIXTURE_DIR")
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

    if rust_dylib.is_file() && c_dylib.is_file() {
        Some((rust_dylib, c_dylib))
    } else {
        None
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

// ============================================================================
// Foreign `extern "C"` GuestSystem Callbacks for Controlled ABI Scenarios
// ============================================================================

#[repr(C)]
struct SpawnBenchCtx {
    pos: BamComponent,
    vel: BamComponent,
    vit: BamComponent,
    arm: BamComponent,
    spawn_count: usize,
}

unsafe extern "C" fn c_abi_dense_kinematics_cb(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: `sys_ctx` is active for this invocation; query index `0` is declared.
    let status = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if status.is_err() {
        return status;
    }

    let mut len: usize = 0;
    // SAFETY: `iter` is valid until callback return; advancing chunk cursor yields `len` rows.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        // SAFETY: Term `0` (`Position`) was declared `AccessMode::WRITE` and is dense `Table`.
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        // SAFETY: Term `1` (`Velocity`) was declared `AccessMode::READ` and is dense `Table`.
        let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) }.cast::<Velocity>();
        if pos_ptr.is_null() || vel_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` points to `len` contiguous initialized `Position` items exclusively borrowed.
        let positions = unsafe { core::slice::from_raw_parts_mut(pos_ptr, len) };
        // SAFETY: `vel_ptr` points to `len` contiguous initialized `Velocity` items shared-borrowed.
        let velocities = unsafe { core::slice::from_raw_parts(vel_ptr, len) };
        for (p, v) in positions.iter_mut().zip(velocities.iter()) {
            p.x += v.x;
            p.y += v.y;
            p.z += v.z;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_changed_filter_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let visited_out = user_data.cast::<usize>();
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: `sys_ctx` is active for this invocation.
    let status = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if status.is_err() {
        return status;
    }

    let mut visited = 0usize;
    let mut acc = 0.0f32;
    let mut len = 0usize;
    // SAFETY: Advancing `iter` over chunks matching `AccessMode::CHANGED` on `Position`.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        // SAFETY: Term `0` (`Position`) has read column access under `AccessMode::CHANGED`.
        let pos_ptr = unsafe { host_iter_column_readonly(iter, 0) }.cast::<Position>();
        if pos_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` points to `len` contiguous `Position` elements for this chunk.
        let slice = unsafe { core::slice::from_raw_parts(pos_ptr, len) };
        for p in slice {
            acc += p.x;
        }
        visited += len;
    }
    black_box(acc);
    if !visited_out.is_null() {
        // SAFETY: Caller points `user_data` at a stack/heap `usize` owned exclusively by this system.
        unsafe {
            *visited_out = visited;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_command_burst_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let armor_handle = BamComponent::from_raw(user_data as usize as u32);
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: Borrowing declared query `0` from active `sys_ctx`.
    let q_res = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if q_res.is_err() {
        return q_res;
    }
    // SAFETY: Borrowing deferred `BamCommands` handle from active `sys_ctx`.
    let c_res = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if c_res.is_err() {
        return c_res;
    }

    let payload = Armor {
        plating: 25.0,
        faction: 7,
    };
    let payload_ptr = (&payload as *const Armor).cast::<u8>();
    let payload_len = core::mem::size_of::<Armor>();

    let mut len = 0usize;
    // SAFETY: Advancing `iter` across matched entity chunks.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        // SAFETY: `host_iter_entities` returns `len` valid `FfiEntity` values for the active chunk.
        let ents_ptr = unsafe { host_iter_entities(iter) };
        // SAFETY: `ents_ptr` is non-null and valid for `len` elements until next chunk advance.
        let ents = unsafe { core::slice::from_raw_parts(ents_ptr, len) };
        for &entity in ents {
            // SAFETY: `cmds` is active and `payload_ptr` points to `size_of::<Armor>()` readable bytes.
            let res =
                unsafe { host_cmd_insert(cmds, entity, armor_handle, payload_ptr, payload_len) };
            if res.is_err() {
                return res;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_sparse_iter_cb(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Borrowing declared query `0` (`Position` dense write + `SparseBuff` sparse write).
    let status = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if status.is_err() {
        return status;
    }

    let mut len = 0usize;
    // SAFETY: Advancing `iter` across batched sparse-set archetype runs.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        // SAFETY: Term `0` (`Position`) is dense `Table` storage declared `WRITE`.
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        // SAFETY: Term `1` (`SparseBuff`) is `SparseSet` storage declared `WRITE`.
        let buff_rows_ptr = unsafe { host_iter_column_rows_mutable(iter, 1) };
        if pos_ptr.is_null() || buff_rows_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` holds `len` contiguous `Position` items in the archetype's table run.
        let positions = unsafe { core::slice::from_raw_parts_mut(pos_ptr, len) };
        // SAFETY: `buff_rows_ptr` holds `len` indirect `*mut u8` row pointers in `SparseRowScratch`.
        let buff_rows = unsafe { core::slice::from_raw_parts(buff_rows_ptr, len) };
        for i in 0..len {
            // SAFETY: Each row pointer addresses a distinct entity's `SparseBuff` value (`8` B, align `4`).
            let buff = unsafe { &mut *buff_rows[i].cast::<SparseBuff>() };
            positions[i].x += buff.bonus;
            buff.remaining = buff.remaining.wrapping_add(1);
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_batched_spawn4_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to an immutable `SpawnBenchCtx` outliving the `App`.
    let cfg = unsafe { &*user_data.cast::<SpawnBenchCtx>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: Borrowing active `BamCommands` handle from `sys_ctx`.
    let status = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if status.is_err() {
        return status;
    }

    let p = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let v = Velocity {
        x: 0.5,
        y: 1.0,
        z: -0.5,
    };
    let vit = Vitality {
        hp: 100.0,
        shield: 50,
    };
    let arm = Armor {
        plating: 12.0,
        faction: 1,
    };

    let comps = [cfg.pos, cfg.vel, cfg.vit, cfg.arm];
    let ptrs = [
        (&p as *const Position).cast::<u8>(),
        (&v as *const Velocity).cast::<u8>(),
        (&vit as *const Vitality).cast::<u8>(),
        (&arm as *const Armor).cast::<u8>(),
    ];

    for _ in 0..cfg.spawn_count {
        let mut out = FfiEntity::NULL;
        // SAFETY: Valid handles and 4 readable payload pointers passed to `host_cmd_spawn`.
        let res = unsafe { host_cmd_spawn(cmds, comps.as_ptr(), ptrs.as_ptr(), 4, &mut out) };
        if res.is_err() {
            return res;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_sequential_spawn4_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to an immutable `SpawnBenchCtx` outliving the `App`.
    let cfg = unsafe { &*user_data.cast::<SpawnBenchCtx>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: Borrowing active `BamCommands` handle from `sys_ctx`.
    let status = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if status.is_err() {
        return status;
    }

    let p = Position {
        x: 1.0,
        y: 2.0,
        z: 3.0,
    };
    let v = Velocity {
        x: 0.5,
        y: 1.0,
        z: -0.5,
    };
    let vit = Vitality {
        hp: 100.0,
        shield: 50,
    };
    let arm = Armor {
        plating: 12.0,
        faction: 1,
    };

    for _ in 0..cfg.spawn_count {
        let mut ent = FfiEntity::NULL;
        // SAFETY: Spawning empty entity via active `cmds`.
        let r0 = unsafe { host_cmd_spawn_empty(cmds, &mut ent) };
        if r0.is_err() {
            return r0;
        }
        // SAFETY: Inserting `Position` payload onto reserved `ent`.
        let r1 = unsafe {
            host_cmd_insert(
                cmds,
                ent,
                cfg.pos,
                (&p as *const Position).cast::<u8>(),
                core::mem::size_of::<Position>(),
            )
        };
        // SAFETY: Inserting `Velocity` payload onto reserved `ent`.
        let r2 = unsafe {
            host_cmd_insert(
                cmds,
                ent,
                cfg.vel,
                (&v as *const Velocity).cast::<u8>(),
                core::mem::size_of::<Velocity>(),
            )
        };
        // SAFETY: Inserting `Vitality` payload onto reserved `ent`.
        let r3 = unsafe {
            host_cmd_insert(
                cmds,
                ent,
                cfg.vit,
                (&vit as *const Vitality).cast::<u8>(),
                core::mem::size_of::<Vitality>(),
            )
        };
        // SAFETY: Inserting `Armor` payload onto reserved `ent`.
        let r4 = unsafe {
            host_cmd_insert(
                cmds,
                ent,
                cfg.arm,
                (&arm as *const Armor).cast::<u8>(),
                core::mem::size_of::<Armor>(),
            )
        };
        if r1.is_err() || r2.is_err() || r3.is_err() || r4.is_err() {
            return FfiResult::INTERNAL_ERROR;
        }
    }
    FfiResult::OK
}

// ============================================================================
// Scenario 1: Dense Iterate (`1k` / `10k` / `100k` Entities)
// ============================================================================

fn bench_dense_iterate(cfg: &BenchConfig, dylibs: Option<&(PathBuf, PathBuf)>) {
    println!("\n================================================================================");
    println!(" SCENARIO 1: Dense SoA Chunk Iteration (1k / 10k / 100k Entities)");
    println!("================================================================================");

    let scales: &[usize] = if cfg.test_mode {
        &[128]
    } else {
        &[1_000, 10_000, 100_000]
    };

    for &count in scales {
        println!(
            "\n--- dense_iterate/{count} ({} measured frames) ---",
            cfg.rounds
        );

        // 1a. Native Bevy (`Position += Velocity`)
        let native_stats = {
            let mut app = App::new();
            for i in 0..count {
                app.world_mut().spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 2.0,
                        z: 0.5,
                    },
                ));
            }
            app.add_systems(Update, |mut q: Query<(&mut Position, &Velocity)>| {
                for (mut p, v) in &mut q {
                    p.x += v.x;
                    p.y += v.y;
                    p.z += v.z;
                }
            });
            measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
        };
        print_row("Native Bevy (Pos + Vel)", native_stats, "ent");

        // 1b. Direct C-ABI `GuestSystem` (`host_iter_column_mutable` + `readonly`)
        let cabi_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let pos_h = app
                .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            let vel_h = app
                .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            for i in 0..count {
                app.world_mut().spawn((
                    Position {
                        x: i as f32,
                        y: 0.0,
                        z: 0.0,
                    },
                    Velocity {
                        x: 1.0,
                        y: 2.0,
                        z: 0.5,
                    },
                ));
            }
            let reg = app.world().resource::<ModRegistry>().clone();
            let spec = BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::new(pos_h, AccessMode::WRITE),
                    BamTermDesc::new(vel_h, AccessMode::READ),
                ],
            };
            let sys = GuestSystem::new(
                "bench_dense_kinematics",
                vec![spec],
                c_abi_dense_kinematics_cb,
                core::ptr::null_mut(),
                reg,
            );
            app.add_systems(Update, sys);
            measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
        };
        print_row("C-ABI GuestSystem (SoA Slice)", cabi_stats, "ent");

        // 1c & 1d. Loaded Asymmetric Rust `.cdylib` & C99 `.dylib` Dynamic Components (when staged)
        if let Some((rust_dylib, c_dylib)) = dylibs {
            let rust_stats = {
                let mut app = App::new();
                app.add_plugins(AbiModPlugin);
                let mod_id = app.load_mod(rust_dylib).expect("load rust fixture");
                let comp_id = lookup_dynamic_component_id(
                    app.world().resource::<ModRegistry>(),
                    "asymmetric_rust_mod::RustModCustomData",
                    "RustModCustomData",
                );
                for i in 0..count {
                    let id = app.world_mut().spawn_empty().id();
                    let init = RawRuntimePayload {
                        primary: i as f32,
                        secondary: 1,
                    };
                    let mut emut = app.world_mut().entity_mut(id);
                    OwningPtr::make(init, |ptr| {
                        // SAFETY: `RawRuntimePayload` matches `RustModCustomData` (`8` B, align `4`).
                        unsafe {
                            emut.insert_by_id(comp_id, ptr);
                        }
                    });
                }
                let s = measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count);
                app.unload_mod(mod_id).expect("unload rust fixture");
                s
            };
            print_row("Asymmetric Rust .cdylib (Dynamic)", rust_stats, "ent");

            let c99_stats = {
                let mut app = App::new();
                app.add_plugins(AbiModPlugin);
                let mod_id = app.load_mod(c_dylib).expect("load c99 fixture");
                let comp_id = lookup_dynamic_component_id(
                    app.world().resource::<ModRegistry>(),
                    "asymmetric_c_mod::CModCustomData",
                    "CModCustomData",
                );
                for i in 0..count {
                    let id = app.world_mut().spawn_empty().id();
                    let init = RawRuntimePayload {
                        primary: i as f32,
                        secondary: 1,
                    };
                    let mut emut = app.world_mut().entity_mut(id);
                    OwningPtr::make(init, |ptr| {
                        // SAFETY: `RawRuntimePayload` matches `CModCustomData` (`8` B, align `4`).
                        unsafe {
                            emut.insert_by_id(comp_id, ptr);
                        }
                    });
                }
                let s = measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count);
                app.unload_mod(mod_id).expect("unload c99 fixture");
                s
            };
            print_row("Asymmetric ISO C99 .dylib (Dynamic)", c99_stats, "ent");
        }
    }
}

// ============================================================================
// Scenario 2: `Changed<Position>` Filter (`1%`, `50%`, `100%` Changed)
// ============================================================================

fn bench_changed_filter(cfg: &BenchConfig) {
    let total_entities = cfg.scale_count(100_000);
    println!("\n================================================================================");
    println!(
        " SCENARIO 2: Changed<Position> Tick Filter ({total_entities} Entities @ 1%, 50%, 100%)"
    );
    println!("================================================================================");

    for &(pct_label, stride) in &[("1%", 100usize), ("50%", 2usize), ("100%", 1usize)] {
        println!("\n--- changed_filter/{pct_label} ({total_entities} entities) ---");
        let expected_changed = total_entities.div_ceil(stride);

        // 2a. Native Bevy `Query<&Position, Changed<Position>>`
        let native_stats = {
            let mut app = App::new();
            let mut entities = Vec::with_capacity(total_entities);
            for i in 0..total_entities {
                entities.push(
                    app.world_mut()
                        .spawn(Position {
                            x: i as f32,
                            y: 1.0,
                            z: 2.0,
                        })
                        .id(),
                );
            }
            app.add_systems(Update, |q: Query<&Position, Changed<Position>>| {
                let mut acc = 0.0f32;
                for p in &q {
                    acc += p.x;
                }
                black_box(acc);
            });
            // Baseline prime tick so initial `Added` ticks clear before partial mutation.
            let _ = timed_update(&mut app);

            measure_fn(
                || {
                    for idx in (0..total_entities).step_by(stride) {
                        if let Some(mut p) = app.world_mut().get_mut::<Position>(entities[idx]) {
                            p.x += 1.0;
                        }
                    }
                    timed_update(&mut app)
                },
                cfg.warmup,
                cfg.rounds,
                total_entities,
            )
        };
        print_row(
            &format!("Native Bevy Changed<Position> ({pct_label})"),
            native_stats,
            "ent",
        );

        // 2b. C-ABI `AccessMode::CHANGED` (`HostIter` table summary skip + tight scan)
        let mut last_visited = Box::new(0usize);
        let cabi_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let pos_h = app
                .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            let mut entities = Vec::with_capacity(total_entities);
            for i in 0..total_entities {
                entities.push(
                    app.world_mut()
                        .spawn(Position {
                            x: i as f32,
                            y: 1.0,
                            z: 2.0,
                        })
                        .id(),
                );
            }
            let reg = app.world().resource::<ModRegistry>().clone();
            let spec = BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::new(pos_h, AccessMode::CHANGED)],
            };
            let visited_ptr = (&mut *last_visited as *mut usize).cast::<c_void>();
            let sys = GuestSystem::new(
                "bench_changed_filter",
                vec![spec],
                c_abi_changed_filter_cb,
                visited_ptr,
                reg,
            );
            app.add_systems(Update, sys);
            let _ = timed_update(&mut app);

            measure_fn(
                || {
                    for idx in (0..total_entities).step_by(stride) {
                        if let Some(mut p) = app.world_mut().get_mut::<Position>(entities[idx]) {
                            p.x += 1.0;
                        }
                    }
                    timed_update(&mut app)
                },
                cfg.warmup,
                cfg.rounds,
                total_entities,
            )
        };
        assert_eq!(*last_visited, expected_changed);
        print_row(
            &format!("C-ABI AccessMode::CHANGED ({pct_label})"),
            cabi_stats,
            "ent",
        );
    }
}

// ============================================================================
// Scenario 3: Command Burst (`10k` `cmd_insert`s / Frame)
// ============================================================================

fn bench_command_burst(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(" SCENARIO 3: Deferred Command Burst ({count} Inserts / Frame)");
    println!("================================================================================");

    // 3a. Native Bevy `Commands::entity(e).insert(Armor)`
    let native_stats = {
        let mut app = App::new();
        for i in 0..count {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Armor::default(),
            ));
        }
        app.add_systems(Update, |mut cmds: Commands, q: Query<Entity>| {
            for e in &q {
                cmds.entity(e).insert(Armor {
                    plating: 25.0,
                    faction: 7,
                });
            }
        });
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    print_row("Native Bevy Commands::insert (10k)", native_stats, "cmd");

    // 3b. C-ABI `host_cmd_insert` (`BamComponent` handle + `CommandArena` reuse)
    let cabi_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let armor_h = app
            .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        for i in 0..count {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                Armor::default(),
            ));
        }
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned {
            flags: 0,
            terms: vec![BamTermDesc::new(pos_h, AccessMode::FILTER_ONLY)],
        };
        let sys = GuestSystem::new(
            "bench_command_burst",
            vec![spec],
            c_abi_command_burst_cb,
            armor_h.to_raw() as usize as *mut c_void,
            reg,
        );
        app.add_systems(Update, sys);
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    print_row("C-ABI host_cmd_insert Arena (10k)", cabi_stats, "cmd");
}

// ============================================================================
// Scenario 4: Sparse-Set Query Iteration (`10k` Entities)
// ============================================================================

fn bench_sparse_query(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(" SCENARIO 4: Batched Sparse-Set Query Iteration ({count} Entities)");
    println!("================================================================================");

    // 4a. Native Bevy `Query<(&mut Position, &mut SparseBuff)>`
    let native_stats = {
        let mut app = App::new();
        for i in 0..count {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                SparseBuff {
                    bonus: 1.5,
                    remaining: 10,
                },
            ));
        }
        app.add_systems(Update, |mut q: Query<(&mut Position, &mut SparseBuff)>| {
            for (mut pos, mut buff) in &mut q {
                pos.x += buff.bonus;
                buff.remaining = buff.remaining.wrapping_add(1);
            }
        });
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    print_row("Native Bevy SparseSet Query", native_stats, "ent");

    // 4b. C-ABI `host_iter_column_rows_mutable` indirect batched sparse rows
    let cabi_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let buff_h = app
            .register_mod_component::<SparseBuff>(
                ComponentOptions::new().with_raw_insert().sparse_set(),
            )
            .unwrap();
        for i in 0..count {
            app.world_mut().spawn((
                Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                },
                SparseBuff {
                    bonus: 1.5,
                    remaining: 10,
                },
            ));
        }
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::new(pos_h, AccessMode::WRITE),
                BamTermDesc::new(buff_h, AccessMode::WRITE),
            ],
        };
        let sys = GuestSystem::new(
            "bench_sparse_iter",
            vec![spec],
            c_abi_sparse_iter_cb,
            core::ptr::null_mut(),
            reg,
        );
        app.add_systems(Update, sys);
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    print_row("C-ABI Indirect Sparse Rows", cabi_stats, "ent");
}

// ============================================================================
// Scenario 5: Batched `cmd_spawn` (4 Components) vs. Sequential Spawn+Insert
// ============================================================================

fn despawn_all_with_position(world: &mut bevy_ecs::world::World) {
    let mut q = world.query_filtered::<Entity, bevy_ecs::query::With<Position>>();
    let entities: Vec<Entity> = q.iter(world).collect();
    for e in entities {
        world.despawn(e);
    }
}

fn bench_spawn_with_4(cfg: &BenchConfig) {
    let spawn_per_frame = cfg.scale_count(2_500);
    println!("\n================================================================================");
    println!(" SCENARIO 5: Spawn-with-4 Components ({spawn_per_frame} Entities / Frame)");
    println!("================================================================================");

    // 5a. Native Bevy Bundle Spawn `(Position, Velocity, Vitality, Armor)`
    let native_stats = {
        let mut app = App::new();
        app.add_systems(Update, move |mut cmds: Commands| {
            for _ in 0..spawn_per_frame {
                cmds.spawn((
                    Position {
                        x: 1.0,
                        y: 2.0,
                        z: 3.0,
                    },
                    Velocity {
                        x: 0.5,
                        y: 1.0,
                        z: -0.5,
                    },
                    Vitality {
                        hp: 100.0,
                        shield: 50,
                    },
                    Armor {
                        plating: 12.0,
                        faction: 1,
                    },
                ));
            }
        });
        measure_fn(
            || {
                let elapsed = timed_update(&mut app);
                despawn_all_with_position(app.world_mut());
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            spawn_per_frame,
        )
    };
    print_row("Native Bevy 4-Component Bundle Spawn", native_stats, "ent");

    // 5b. C-ABI Batched `host_cmd_spawn` (`insert_by_ids` single archetype move)
    let mut batched_ctx = Box::new(SpawnBenchCtx {
        pos: BamComponent::INVALID,
        vel: BamComponent::INVALID,
        vit: BamComponent::INVALID,
        arm: BamComponent::INVALID,
        spawn_count: spawn_per_frame,
    });
    let batched_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        batched_ctx.pos = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        batched_ctx.vel = app
            .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        batched_ctx.vit = app
            .register_mod_component::<Vitality>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        batched_ctx.arm = app
            .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
            .unwrap();

        let reg = app.world().resource::<ModRegistry>().clone();
        let ctx_ptr = (&mut *batched_ctx as *mut SpawnBenchCtx).cast::<c_void>();
        let sys = GuestSystem::new(
            "bench_batched_spawn4",
            Vec::new(),
            c_abi_batched_spawn4_cb,
            ctx_ptr,
            reg,
        );
        app.add_systems(Update, sys);
        measure_fn(
            || {
                let elapsed = timed_update(&mut app);
                despawn_all_with_position(app.world_mut());
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            spawn_per_frame,
        )
    };
    print_row(
        "C-ABI Batched host_cmd_spawn (4 comps)",
        batched_stats,
        "ent",
    );

    // 5c. C-ABI Sequential `host_cmd_spawn_empty` + 4x `host_cmd_insert` (4 archetype moves)
    let mut seq_ctx = Box::new(SpawnBenchCtx {
        pos: BamComponent::INVALID,
        vel: BamComponent::INVALID,
        vit: BamComponent::INVALID,
        arm: BamComponent::INVALID,
        spawn_count: spawn_per_frame,
    });
    let seq_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        seq_ctx.pos = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        seq_ctx.vel = app
            .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        seq_ctx.vit = app
            .register_mod_component::<Vitality>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        seq_ctx.arm = app
            .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
            .unwrap();

        let reg = app.world().resource::<ModRegistry>().clone();
        let ctx_ptr = (&mut *seq_ctx as *mut SpawnBenchCtx).cast::<c_void>();
        let sys = GuestSystem::new(
            "bench_seq_spawn4",
            Vec::new(),
            c_abi_sequential_spawn4_cb,
            ctx_ptr,
            reg,
        );
        app.add_systems(Update, sys);
        measure_fn(
            || {
                let elapsed = timed_update(&mut app);
                despawn_all_with_position(app.world_mut());
                elapsed
            },
            cfg.warmup,
            cfg.rounds,
            spawn_per_frame,
        )
    };
    print_row("C-ABI Sequential spawn_empty + 4x insert", seq_stats, "ent");

    let speedup = seq_stats.median_ms / batched_stats.median_ms.max(1e-9);
    println!("  -> Batched cmd_spawn speedup over sequential spawn+insert: {speedup:.2}x");
}

fn main() {
    let cfg = BenchConfig::from_args();
    let dylibs = try_get_dylib_paths();

    println!("================================================================================");
    println!(
        " bevy_abi_mod Canonical Benchmark Suite (ABI 0.2.0) [mode={}]",
        if cfg.test_mode {
            "smoke-test"
        } else if cfg.quick_mode {
            "quick"
        } else {
            "full"
        }
    );
    if dylibs.is_none() {
        println!(
            " [Note] External .cdylib/.dylib fixtures not staged in BAM_FIXTURE_DIR; \
             running in-process Native + C-ABI scenarios (run `cargo xtask bench` for external mods)."
        );
    }
    println!("================================================================================");

    if cfg.matches("dense_iterate") {
        bench_dense_iterate(&cfg, dylibs.as_ref());
    }
    if cfg.matches("changed_filter") {
        bench_changed_filter(&cfg);
    }
    if cfg.matches("command_burst") {
        bench_command_burst(&cfg);
    }
    if cfg.matches("sparse_query") {
        bench_sparse_query(&cfg);
    }
    if cfg.matches("spawn_with_4") {
        bench_spawn_with_4(&cfg);
    }

    println!("\nCanonical benchmark suite completed cleanly.");
}
