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
//! 6. **`query_filters`** — Baseline vs `Allow<Disabled>` (`AccessMode::ALLOW`), `Has<T>`
//!    (`AccessMode::HAS` `[FfiBool]` mask), and `Or<(...)>` (`or_group`) union filters at `10k`.
//! 7. **`batch_commands`** — `host_cmd_spawn_batch` SoA bulk spawn vs per-entity `host_cmd_spawn`
//!    loop (`1k` & `10k` entities) plus `host_cmd_despawn_all` vs per-entity `host_cmd_despawn`.
//! 8. **`summary_tick`** — `BAM_COMPONENT_FLAG_SUMMARY_TICK` `Changed<T>` table-skip throughput at
//!    `0%`, `1%`, and `100%` dirty ratios (`10k` entities across `100` tables).
//! 9. **`entity_children`** — Zero-copy `host_sys_entity_children` (`&[FfiEntity]`) parent->child
//!    fan-out traversal (`1k` parents × `10` = `10k` edges) vs Native Bevy `Query<&Children>`.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::cast_ptr_alignment, unreachable_pub)]

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HostWireBridge, LoadContext, host_cmd_despawn,
    host_cmd_despawn_all, host_cmd_insert, host_cmd_spawn, host_cmd_spawn_batch,
    host_cmd_spawn_empty, host_iter_column_mutable, host_iter_column_readonly,
    host_iter_column_rows_mutable, host_iter_entities, host_iter_next_chunk,
    host_register_component, host_sys_commands, host_sys_entity_children, host_sys_query,
};
use bevy_abi_mod_sys::{
    AccessMode, BAM_CHILDREN_NAME, BAM_COMPONENT_FLAG_SUMMARY_TICK, BamCommands, BamComponent,
    BamIter, BamSystemCtx, BamTermDesc, ComponentDesc, ComponentUuid, FfiBool, FfiEntity,
    FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::component::{Component, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::entity_disabling::Disabled;
use bevy_ecs::hierarchy::{ChildOf, Children};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::query::{Allow, Changed, Has, Or, With};
use bevy_ecs::system::{Commands, Query};
use core::ffi::c_void;
use std::env;
use std::hint::black_box;
use std::path::PathBuf;
use std::sync::{Arc, atomic::AtomicBool};
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

/// Zero-sized filter marker tag A (`StorageType::Table`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, ModComponent)]
#[mod_component(name = "bench::FilterTagA")]
pub struct FilterTagA;

/// Zero-sized filter marker tag B (`StorageType::Table`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Default, ModComponent)]
#[mod_component(name = "bench::FilterTagB")]
pub struct FilterTagB;

/// Archetype fan-out bit markers used to partition 10k entities across 100 distinct tables.
#[derive(Component, Default, Clone, Copy)]
struct TableBit0;
#[derive(Component, Default, Clone, Copy)]
struct TableBit1;
#[derive(Component, Default, Clone, Copy)]
struct TableBit2;
#[derive(Component, Default, Clone, Copy)]
struct TableBit3;
#[derive(Component, Default, Clone, Copy)]
struct TableBit4;
#[derive(Component, Default, Clone, Copy)]
struct TableBit5;
#[derive(Component, Default, Clone, Copy)]
struct TableBit6;

struct DisabledBridge;

impl HostWireBridge<Disabled> for DisabledBridge {
    type Wire = FfiBool;
    const CANONICAL_NAME: &'static str = "bevy_ecs::entity_disabling::Disabled";
    const ALIAS: Option<&'static str> = Some("Disabled");
    const SUPPORTS_SET: bool = false;

    fn to_wire(_value: &Disabled) -> Self::Wire {
        FfiBool::TRUE
    }
}

fn register_disabled_bridge(app: &mut App) -> BamComponent {
    let cid = app.world_mut().register_component::<Disabled>();
    let mut reg = app.world_mut().resource_mut::<ModRegistry>();
    reg.register_bridged_component::<Disabled, DisabledBridge>(cid, 0, None)
        .expect("register Disabled bridge")
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

// ============================================================================
// Scenario 6: Wave 1 Query Filter Throughput (`Allow`, `Has`, `Or` @ 10k)
// ============================================================================

unsafe extern "C" fn c_abi_query_allow_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Query 0 (`&mut Position`, `&Velocity`, `Allow<Disabled>`) is registered on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut total = 0usize;
    let mut len = 0usize;
    // SAFETY: Iterates dense matched tables in `iter`.
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        // SAFETY: Term 0 is writable dense `Position`.
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        // SAFETY: Term 1 is readonly dense `Velocity`.
        let vel_ptr = unsafe { host_iter_column_readonly(iter, 1) }.cast::<Velocity>();
        if pos_ptr.is_null() || vel_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` and `vel_ptr` are valid disjoint slices of length `len`.
        let pos = unsafe { core::slice::from_raw_parts_mut(pos_ptr, len) };
        // SAFETY: `vel_ptr` covers `len` elements.
        let vel = unsafe { core::slice::from_raw_parts(vel_ptr, len) };
        for i in 0..len {
            pos[i].x += vel[i].x;
        }
        total += len;
    }
    if !user_data.is_null() {
        // SAFETY: `user_data` points to `usize` counter in caller.
        unsafe { *user_data.cast::<usize>() = total };
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_query_has_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Query 0 (`&mut Position`, `Has<FilterTagA>`) is registered on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut tagged = 0usize;
    let mut len = 0usize;
    // SAFETY: Iterates matched tables in `iter`.
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        // SAFETY: Term 0 is writable dense `Position`.
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        // SAFETY: Term 1 (`AccessMode::HAS`) yields synthesized `[FfiBool; len]`.
        let has_ptr = unsafe { host_iter_column_readonly(iter, 1) }.cast::<FfiBool>();
        if pos_ptr.is_null() || has_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` covers `len` `Position` elements.
        let pos = unsafe { core::slice::from_raw_parts_mut(pos_ptr, len) };
        // SAFETY: `has_ptr` covers `len` `FfiBool` entries.
        let has = unsafe { core::slice::from_raw_parts(has_ptr, len) };
        for i in 0..len {
            if has[i].get() {
                pos[i].x += 1.0;
                tagged += 1;
            }
        }
    }
    if !user_data.is_null() {
        // SAFETY: `user_data` points to `usize` counter in caller.
        unsafe { *user_data.cast::<usize>() = tagged };
    }
    FfiResult::OK
}

unsafe extern "C" fn c_abi_query_or_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Query 0 (`&mut Position`, `Or<(With<FilterTagA>, With<FilterTagB>)>`) is registered.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut total = 0usize;
    let mut len = 0usize;
    // SAFETY: Iterates matched tables in `iter`.
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        // SAFETY: Term 0 is writable dense `Position`.
        let pos_ptr = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        if pos_ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `pos_ptr` covers `len` `Position` elements.
        let pos = unsafe { core::slice::from_raw_parts_mut(pos_ptr, len) };
        for p in pos.iter_mut() {
            p.x += 1.0;
        }
        total += len;
    }
    if !user_data.is_null() {
        // SAFETY: `user_data` points to `usize` counter in caller.
        unsafe { *user_data.cast::<usize>() = total };
    }
    FfiResult::OK
}

fn spawn_filter_bench_world(world: &mut bevy_ecs::world::World, count: usize) {
    // 50% `(Pos, Vel, FilterTagA)`, 25% `(Pos, Vel, FilterTagB)`, 25% `(Pos, Vel, FilterTagA, Disabled)`.
    for i in 0..count {
        let pos = Position {
            x: i as f32,
            y: 1.0,
            z: 2.0,
        };
        let vel = Velocity {
            x: 0.5,
            y: 0.0,
            z: 0.0,
        };
        match i % 4 {
            0 | 1 => {
                world.spawn((pos, vel, FilterTagA));
            }
            2 => {
                world.spawn((pos, vel, FilterTagB));
            }
            _ => {
                world.spawn((pos, vel, FilterTagA, Disabled));
            }
        }
    }
}

fn bench_query_filters(cfg: &BenchConfig) {
    let count = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(
        " SCENARIO 6: Wave 1 Query Filters (Baseline vs Allow<Disabled>, Has<T>, Or<...>, {count} ents)"
    );
    println!("================================================================================");

    // 6a. Baseline C-ABI (&mut Position, &Velocity) — excludes Disabled (75% visible)
    let expected_enabled = (count * 3) / 4;
    let mut base_visited = Box::new(0usize);
    let base_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        spawn_filter_bench_world(app.world_mut(), count);
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(vel_h, AccessMode::READ),
        ]);
        let ptr = (&mut *base_visited as *mut usize).cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new(
                "bench_filter_base",
                vec![spec],
                c_abi_query_allow_cb,
                ptr,
                reg,
            ),
        );
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            expected_enabled,
        )
    };
    assert_eq!(*base_visited, expected_enabled);
    print_row("C-ABI Baseline (Pos, Vel) [enabled]", base_stats, "ent");

    // 6b. Native Allow<Disabled> vs C-ABI Allow<Disabled> (yields 100% = `count`)
    let native_allow_stats = {
        let mut app = App::new();
        spawn_filter_bench_world(app.world_mut(), count);
        app.add_systems(
            Update,
            |mut q: Query<(&mut Position, &Velocity), Allow<Disabled>>| {
                for (mut p, v) in &mut q {
                    p.x += v.x;
                }
            },
        );
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    print_row("Native Bevy Allow<Disabled>", native_allow_stats, "ent");

    let mut allow_visited = Box::new(0usize);
    let cabi_allow_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let vel_h = app
            .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let disabled_h = register_disabled_bridge(&mut app);
        spawn_filter_bench_world(app.world_mut(), count);
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(vel_h, AccessMode::READ),
            BamTermDesc::new(disabled_h, AccessMode::ALLOW),
        ]);
        let ptr = (&mut *allow_visited as *mut usize).cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new(
                "bench_filter_allow",
                vec![spec],
                c_abi_query_allow_cb,
                ptr,
                reg,
            ),
        );
        measure_fn(|| timed_update(&mut app), cfg.warmup, cfg.rounds, count)
    };
    assert_eq!(*allow_visited, count);
    print_row(
        "C-ABI AccessMode::ALLOW (Disabled)",
        cabi_allow_stats,
        "ent",
    );

    // 6c. Native Has<FilterTagA> vs C-ABI AccessMode::HAS
    let native_has_stats = {
        let mut app = App::new();
        spawn_filter_bench_world(app.world_mut(), count);
        app.add_systems(Update, |mut q: Query<(&mut Position, Has<FilterTagA>)>| {
            for (mut p, has_a) in &mut q {
                if has_a {
                    p.x += 1.0;
                }
            }
        });
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            expected_enabled,
        )
    };
    print_row("Native Bevy Has<FilterTagA>", native_has_stats, "ent");

    let mut has_tagged = Box::new(0usize);
    let cabi_has_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let tag_a_h = app
            .register_mod_component::<FilterTagA>(ComponentOptions::new())
            .unwrap();
        spawn_filter_bench_world(app.world_mut(), count);
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(tag_a_h, AccessMode::HAS),
        ]);
        let ptr = (&mut *has_tagged as *mut usize).cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new("bench_filter_has", vec![spec], c_abi_query_has_cb, ptr, reg),
        );
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            expected_enabled,
        )
    };
    assert_eq!(*has_tagged, count / 2);
    print_row(
        "C-ABI AccessMode::HAS ([FfiBool] mask)",
        cabi_has_stats,
        "ent",
    );

    // 6d. Native Or<(With<FilterTagA>, With<FilterTagB>)> vs C-ABI or_group
    let native_or_stats = {
        let mut app = App::new();
        spawn_filter_bench_world(app.world_mut(), count);
        app.add_systems(
            Update,
            |mut q: Query<&mut Position, Or<(With<FilterTagA>, With<FilterTagB>)>>| {
                for mut p in &mut q {
                    p.x += 1.0;
                }
            },
        );
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            expected_enabled,
        )
    };
    print_row("Native Bevy Or<(With<A>, With<B>)>", native_or_stats, "ent");

    let mut or_visited = Box::new(0usize);
    let cabi_or_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        let pos_h = app
            .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
            .unwrap();
        let tag_a_h = app
            .register_mod_component::<FilterTagA>(ComponentOptions::new())
            .unwrap();
        let tag_b_h = app
            .register_mod_component::<FilterTagB>(ComponentOptions::new())
            .unwrap();
        spawn_filter_bench_world(app.world_mut(), count);
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![
            BamTermDesc::new(pos_h, AccessMode::WRITE),
            BamTermDesc::new(tag_a_h, AccessMode::FILTER_ONLY).with_or_group(1),
            BamTermDesc::new(tag_b_h, AccessMode::FILTER_ONLY).with_or_group(1),
        ]);
        let ptr = (&mut *or_visited as *mut usize).cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new("bench_filter_or", vec![spec], c_abi_query_or_cb, ptr, reg),
        );
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            expected_enabled,
        )
    };
    assert_eq!(*or_visited, expected_enabled);
    print_row("C-ABI or_group (With<A> || With<B>)", cabi_or_stats, "ent");
}

// ============================================================================
// Scenario 7: Wave 2 Batch Commands (`cmd_spawn_batch` & `cmd_despawn_all`)
// ============================================================================

struct SoaSpawnBenchCtx {
    comps: [BamComponent; 4],
    pos_col: Vec<Position>,
    vel_col: Vec<Velocity>,
    vit_col: Vec<Vitality>,
    arm_col: Vec<Armor>,
    out_entities: Vec<FfiEntity>,
    count: usize,
}

unsafe extern "C" fn c_abi_soa_spawn_batch_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `SoaSpawnBenchCtx` owned by the benchmark loop.
    let cfg = unsafe { &mut *user_data.cast::<SoaSpawnBenchCtx>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: `sys_ctx` is active.
    let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if rc.is_err() {
        return rc;
    }
    let ptrs = [
        cfg.pos_col.as_ptr().cast::<u8>(),
        cfg.vel_col.as_ptr().cast::<u8>(),
        cfg.vit_col.as_ptr().cast::<u8>(),
        cfg.arm_col.as_ptr().cast::<u8>(),
    ];
    // SAFETY: `cmds` is active and all 4 SoA columns have length `cfg.count`.
    unsafe {
        host_cmd_spawn_batch(
            cmds,
            cfg.comps.as_ptr(),
            ptrs.as_ptr(),
            4,
            cfg.count,
            cfg.out_entities.as_mut_ptr(),
        )
    }
}

struct DespawnBenchCtx {
    tag_h: BamComponent,
}

unsafe extern "C" fn c_abi_despawn_all_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `DespawnBenchCtx`.
    let cfg = unsafe { &*user_data.cast::<DespawnBenchCtx>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: `sys_ctx` is active.
    let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if rc.is_err() {
        return rc;
    }
    let terms = [BamTermDesc::new(cfg.tag_h, AccessMode::FILTER_ONLY)];
    // SAFETY: `cmds` is active and `terms` is a valid 1-element filter slice.
    unsafe { host_cmd_despawn_all(cmds, terms.as_ptr(), 1) }
}

unsafe extern "C" fn c_abi_despawn_loop_cb(
    sys_ctx: *mut BamSystemCtx,
    _user_data: *mut c_void,
) -> FfiResult {
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: `sys_ctx` is active.
    let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    if rc.is_err() {
        return rc;
    }
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Query 0 (`With<FilterTagA>`) is registered on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut len = 0usize;
    // SAFETY: Iterates matched chunks.
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        // SAFETY: `entities` covers `len` `FfiEntity` values.
        let ents = unsafe { host_iter_entities(iter) };
        if ents.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `ents` slice covers `len` elements.
        let slice = unsafe { core::slice::from_raw_parts(ents, len) };
        for &e in slice {
            // SAFETY: Queues individual `host_cmd_despawn`.
            let r = unsafe { host_cmd_despawn(cmds, e) };
            if r.is_err() {
                return r;
            }
        }
    }
    FfiResult::OK
}

fn bench_batch_commands(cfg: &BenchConfig) {
    let scales: &[usize] = if cfg.test_mode {
        &[64]
    } else {
        &[1_000, 10_000]
    };
    println!("\n================================================================================");
    println!(
        " SCENARIO 7: Batch Commands (`cmd_spawn_batch` SoA vs `cmd_spawn` loop & `cmd_despawn_all`)"
    );
    println!("================================================================================");

    for &count in scales {
        println!(
            "\n--- batch_commands/{count} ({} measured frames) ---",
            cfg.rounds
        );

        // 7a. Native Bevy `Commands::spawn_batch`
        let native_spawn_stats = {
            let mut app = App::new();
            app.add_systems(Update, move |mut cmds: Commands| {
                cmds.spawn_batch((0..count).map(|i| {
                    (
                        Position {
                            x: i as f32,
                            y: 0.0,
                            z: 0.0,
                        },
                        Velocity {
                            x: 1.0,
                            y: 0.5,
                            z: 0.0,
                        },
                        Vitality {
                            hp: 100.0,
                            shield: 50,
                        },
                        Armor {
                            plating: 12.0,
                            faction: 1,
                        },
                    )
                }));
            });
            measure_fn(
                || {
                    let elapsed = timed_update(&mut app);
                    despawn_all_with_position(app.world_mut());
                    elapsed
                },
                cfg.warmup,
                cfg.rounds,
                count,
            )
        };
        print_row(
            &format!("Native Bevy Commands::spawn_batch ({count})"),
            native_spawn_stats,
            "ent",
        );

        // 7b. C-ABI `host_cmd_spawn_batch` (SoA 4-column bulk spawn in 1 FFI call)
        let mut soa_ctx = Box::new(SoaSpawnBenchCtx {
            comps: [BamComponent::INVALID; 4],
            pos_col: (0..count)
                .map(|i| Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                })
                .collect(),
            vel_col: vec![
                Velocity {
                    x: 1.0,
                    y: 0.5,
                    z: 0.0,
                };
                count
            ],
            vit_col: vec![
                Vitality {
                    hp: 100.0,
                    shield: 50,
                };
                count
            ],
            arm_col: vec![
                Armor {
                    plating: 12.0,
                    faction: 1,
                };
                count
            ],
            out_entities: vec![FfiEntity::NULL; count],
            count,
        });
        let soa_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            soa_ctx.comps = [
                app.register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
                    .unwrap(),
                app.register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
                    .unwrap(),
                app.register_mod_component::<Vitality>(ComponentOptions::new().with_raw_insert())
                    .unwrap(),
                app.register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
                    .unwrap(),
            ];
            let reg = app.world().resource::<ModRegistry>().clone();
            let ptr = (&mut *soa_ctx as *mut SoaSpawnBenchCtx).cast::<c_void>();
            app.add_systems(
                Update,
                GuestSystem::new(
                    "bench_soa_spawn",
                    Vec::new(),
                    c_abi_soa_spawn_batch_cb,
                    ptr,
                    reg,
                ),
            );
            measure_fn(
                || {
                    let elapsed = timed_update(&mut app);
                    despawn_all_with_position(app.world_mut());
                    elapsed
                },
                cfg.warmup,
                cfg.rounds,
                count,
            )
        };
        print_row(
            &format!("C-ABI host_cmd_spawn_batch SoA ({count})"),
            soa_stats,
            "ent",
        );

        // 7c. C-ABI per-entity `host_cmd_spawn` loop (4 components each)
        let mut loop_ctx = Box::new(SpawnBenchCtx {
            pos: BamComponent::INVALID,
            vel: BamComponent::INVALID,
            vit: BamComponent::INVALID,
            arm: BamComponent::INVALID,
            spawn_count: count,
        });
        let loop_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            loop_ctx.pos = app
                .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            loop_ctx.vel = app
                .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            loop_ctx.vit = app
                .register_mod_component::<Vitality>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            loop_ctx.arm = app
                .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
                .unwrap();
            let reg = app.world().resource::<ModRegistry>().clone();
            let ptr = (&mut *loop_ctx as *mut SpawnBenchCtx).cast::<c_void>();
            app.add_systems(
                Update,
                GuestSystem::new(
                    "bench_loop_spawn",
                    Vec::new(),
                    c_abi_batched_spawn4_cb,
                    ptr,
                    reg,
                ),
            );
            measure_fn(
                || {
                    let elapsed = timed_update(&mut app);
                    despawn_all_with_position(app.world_mut());
                    elapsed
                },
                cfg.warmup,
                cfg.rounds,
                count,
            )
        };
        print_row(
            &format!("C-ABI host_cmd_spawn loop ({count})"),
            loop_stats,
            "ent",
        );

        let spawn_speedup = loop_stats.median_ms / soa_stats.median_ms.max(1e-9);
        println!("  -> cmd_spawn_batch SoA speedup over cmd_spawn loop: {spawn_speedup:.2}x");

        // 7d. `cmd_despawn_all` vs per-entity `cmd_despawn` loop
        let despawn_all_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let tag_h = app
                .register_mod_component::<FilterTagA>(ComponentOptions::new())
                .unwrap();
            let ctx = Box::new(DespawnBenchCtx { tag_h });
            let ptr = Box::into_raw(ctx).cast::<c_void>();
            let reg = app.world().resource::<ModRegistry>().clone();
            app.add_systems(
                Update,
                GuestSystem::new(
                    "bench_despawn_all",
                    Vec::new(),
                    c_abi_despawn_all_cb,
                    ptr,
                    reg,
                ),
            );
            let res = measure_fn(
                || {
                    app.world_mut().spawn_batch(vec![FilterTagA; count]);
                    timed_update(&mut app)
                },
                cfg.warmup,
                cfg.rounds,
                count,
            );
            // SAFETY: `ptr` came from `Box::into_raw(ctx)` above and system is no longer running.
            let _ = unsafe { Box::from_raw(ptr.cast::<DespawnBenchCtx>()) };
            res
        };
        print_row(
            &format!("C-ABI host_cmd_despawn_all ({count})"),
            despawn_all_stats,
            "ent",
        );

        let despawn_loop_stats = {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let tag_h = app
                .register_mod_component::<FilterTagA>(ComponentOptions::new())
                .unwrap();
            let reg = app.world().resource::<ModRegistry>().clone();
            let spec =
                BamQuerySpecOwned::new(vec![BamTermDesc::new(tag_h, AccessMode::FILTER_ONLY)]);
            app.add_systems(
                Update,
                GuestSystem::new(
                    "bench_despawn_loop",
                    vec![spec],
                    c_abi_despawn_loop_cb,
                    core::ptr::null_mut(),
                    reg,
                ),
            );
            measure_fn(
                || {
                    app.world_mut().spawn_batch(vec![FilterTagA; count]);
                    timed_update(&mut app)
                },
                cfg.warmup,
                cfg.rounds,
                count,
            )
        };
        print_row(
            &format!("C-ABI host_cmd_despawn loop ({count})"),
            despawn_loop_stats,
            "ent",
        );
    }
}

// ============================================================================
// Scenario 8: `BAM_COMPONENT_FLAG_SUMMARY_TICK` Multi-Table Skip (10k / 100 tables)
// ============================================================================

fn spawn_100_table_entities(
    world: &mut bevy_ecs::world::World,
    comp_id: ComponentId,
    count: usize,
) -> Vec<Entity> {
    let mut entities = Vec::with_capacity(count);
    let tables = 100usize;
    for i in 0..count {
        let tid = i % tables;
        let mut e = world.spawn_empty();
        // SAFETY: `comp_id` is a 12-byte `(f32, f32, f32)` dynamic component registered in `world`.
        unsafe {
            OwningPtr::make([i as f32, 1.0f32, 2.0f32], |ptr| {
                e.insert_by_id(comp_id, ptr);
            });
        }
        if (tid & 1) != 0 {
            e.insert(TableBit0);
        }
        if (tid & 2) != 0 {
            e.insert(TableBit1);
        }
        if (tid & 4) != 0 {
            e.insert(TableBit2);
        }
        if (tid & 8) != 0 {
            e.insert(TableBit3);
        }
        if (tid & 16) != 0 {
            e.insert(TableBit4);
        }
        if (tid & 32) != 0 {
            e.insert(TableBit5);
        }
        if (tid & 64) != 0 {
            e.insert(TableBit6);
        }
        entities.push(e.id());
    }
    entities
}

fn mark_dynamic_rows_dirty(
    world: &mut bevy_ecs::world::World,
    comp_id: ComponentId,
    entities: &[Entity],
    pct: usize,
) {
    if pct == 0 {
        return;
    }
    if pct == 1 {
        // Dirty table 0 (`entities` at indices `0, 100, 200, ...`, i.e., 1% of tables & 1% of rows).
        for &e in entities.iter().step_by(100) {
            if let Ok(mut em) = world.get_entity_mut(e)
                && let Ok(untyped) = em.get_mut_by_id(comp_id)
            {
                // SAFETY: `comp_id` holds `[f32; 3]`; `Mut<[f32; 3]>` also updates `summary_tick`
                // (unlike `MutUntyped::set_changed` in Bevy 0.20.1).
                let mut arr = unsafe { untyped.with_type::<[f32; 3]>() };
                arr[0] += 1.0;
            }
        }
        return;
    }
    for &e in entities {
        if let Ok(mut em) = world.get_entity_mut(e)
            && let Ok(untyped) = em.get_mut_by_id(comp_id)
        {
            // SAFETY: `comp_id` holds `[f32; 3]`.
            let mut arr = unsafe { untyped.with_type::<[f32; 3]>() };
            arr[0] += 1.0;
        }
    }
}

fn bench_summary_tick(cfg: &BenchConfig) {
    let total_entities = cfg.scale_count(10_000);
    println!("\n================================================================================");
    println!(
        " SCENARIO 8: `BAM_COMPONENT_FLAG_SUMMARY_TICK` Table-Skip ({total_entities} ents, 100 tables)"
    );
    println!("================================================================================");

    let cases: [(&str, usize); 3] = [("0%", 0), ("1%", 1), ("100%", 100)];

    for (label, pct) in cases {
        println!(
            "\n--- summary_tick/{label} ({} measured frames) ---",
            cfg.rounds
        );

        for (variant_label, use_summary) in [
            ("Non-SummaryTick Changed<T>", false),
            ("SummaryTick (FLAG_SUMMARY_TICK) Changed<T>", true),
        ] {
            let mut last_visited = Box::new(0usize);
            let stats = {
                let mut app = App::new();
                app.add_plugins(AbiModPlugin);
                let mut h = BamComponent::INVALID;
                let cname = if use_summary {
                    c"bench::DynSummaryPos"
                } else {
                    c"bench::DynPlainPos"
                };
                let uuid = ComponentUuid::from_parts(
                    if use_summary {
                        "bench::DynSummaryPos"
                    } else {
                        "bench::DynPlainPos"
                    },
                    12,
                    4,
                    if use_summary { 1 } else { 2 },
                );
                let mut desc = ComponentDesc::new(cname.as_ptr(), uuid, 12, 4);
                if use_summary {
                    desc = desc.with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK);
                }
                {
                    let reg = app.world().resource::<ModRegistry>().clone();
                    let active = Arc::new(AtomicBool::new(true));
                    let mut lctx = LoadContext::with_world_and_mod_id(
                        reg,
                        active,
                        app.world_mut() as *mut bevy_ecs::world::World,
                        None,
                    );
                    // SAFETY: Valid `LoadContext` and `ComponentDesc`.
                    let rc = unsafe { host_register_component(lctx.as_ffi_ptr(), &desc, &mut h) };
                    assert_eq!(rc, FfiResult::OK);
                    let updated = lctx.registry.clone();
                    lctx.finish_init();
                    *app.world_mut().resource_mut::<ModRegistry>() = updated;
                }
                let comp_id = app
                    .world()
                    .resource::<ModRegistry>()
                    .get_by_handle(h)
                    .unwrap()
                    .component_id;
                let entities = spawn_100_table_entities(app.world_mut(), comp_id, total_entities);
                let reg = app.world().resource::<ModRegistry>().clone();
                let spec = BamQuerySpecOwned::new(vec![BamTermDesc::new(h, AccessMode::CHANGED)]);
                let visited_ptr = (&mut *last_visited as *mut usize).cast::<c_void>();
                app.add_systems(
                    Update,
                    GuestSystem::new(
                        "bench_summary_tick",
                        vec![spec],
                        c_abi_changed_filter_cb,
                        visited_ptr,
                        reg,
                    ),
                );
                let _ = timed_update(&mut app);

                measure_fn(
                    || {
                        mark_dynamic_rows_dirty(app.world_mut(), comp_id, &entities, pct);
                        timed_update(&mut app)
                    },
                    cfg.warmup,
                    cfg.rounds,
                    total_entities,
                )
            };
            let expected = match pct {
                0 => 0,
                1 => total_entities.div_ceil(100),
                _ => total_entities,
            };
            assert_eq!(*last_visited, expected);
            print_row(&format!("{variant_label} ({label})"), stats, "ent");
        }
    }
}

// ============================================================================
// Scenario 9: Zero-Copy `sys_entity_children` Fan-Out Traversal
// ============================================================================

struct ChildrenBenchCtx {
    children_h: BamComponent,
    edges_visited: usize,
    checksum: u64,
}

unsafe extern "C" fn c_abi_entity_children_cb(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `ChildrenBenchCtx`.
    let state = unsafe { &mut *user_data.cast::<ChildrenBenchCtx>() };
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Query 0 (`&Children`) is registered on `sys_ctx`.
    let rc = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    if rc.is_err() {
        return rc;
    }
    let mut edges = 0usize;
    let mut sum = 0u64;
    let mut len = 0usize;
    // SAFETY: Iterates matched parent chunks.
    while unsafe { host_iter_next_chunk(iter, &mut len) }.get() {
        // SAFETY: `entities` covers `len` parent handles.
        let ents = unsafe { host_iter_entities(iter) };
        if ents.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `ents` covers `len` elements.
        let parents = unsafe { core::slice::from_raw_parts(ents, len) };
        for &parent in parents {
            let mut children_ptr: *const FfiEntity = core::ptr::null();
            let mut children_len = 0usize;
            // SAFETY: Query 0 declared read access on `Children` (`state.children_h`).
            let r = unsafe {
                host_sys_entity_children(
                    sys_ctx,
                    parent,
                    state.children_h,
                    &mut children_ptr,
                    &mut children_len,
                )
            };
            if r.is_err() {
                return r;
            }
            if children_len > 0 && !children_ptr.is_null() {
                // SAFETY: `children_ptr` is a zero-copy `&[FfiEntity]` slice of `Children` storage.
                let children = unsafe { core::slice::from_raw_parts(children_ptr, children_len) };
                for &c in children {
                    sum = sum.wrapping_add(c.to_bits());
                }
                edges += children_len;
            }
        }
    }
    state.edges_visited = edges;
    state.checksum = sum;
    FfiResult::OK
}

fn spawn_hierarchy_fanout(
    world: &mut bevy_ecs::world::World,
    parents: usize,
    children_per_parent: usize,
) {
    for _ in 0..parents {
        let p = world.spawn(FilterTagA).id();
        for _ in 0..children_per_parent {
            world.spawn(ChildOf(p));
        }
    }
}

fn bench_entity_children(cfg: &BenchConfig) {
    let parents = if cfg.test_mode { 20 } else { 1_000 };
    let children_per_parent = 10usize;
    let total_edges = parents * children_per_parent;
    println!("\n================================================================================");
    println!(
        " SCENARIO 9: Zero-Copy `sys_entity_children` Traversal ({parents} parents x {children_per_parent} = {total_edges} edges)"
    );
    println!("================================================================================");

    // 9a. Native Bevy `Query<&Children>`
    let native_stats = {
        let mut app = App::new();
        spawn_hierarchy_fanout(app.world_mut(), parents, children_per_parent);
        app.add_systems(Update, |q: Query<&Children>| {
            let mut sum = 0u64;
            for children in &q {
                for &child in children {
                    sum = sum.wrapping_add(child.to_bits());
                }
            }
            black_box(sum);
        });
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            total_edges,
        )
    };
    print_row("Native Bevy Query<&Children>", native_stats, "edge");

    // 9b. C-ABI `host_sys_entity_children` zero-copy `&[FfiEntity]`
    let mut ctx = Box::new(ChildrenBenchCtx {
        children_h: BamComponent::INVALID,
        edges_visited: 0,
        checksum: 0,
    });
    let cabi_stats = {
        let mut app = App::new();
        app.add_plugins(AbiModPlugin);
        spawn_hierarchy_fanout(app.world_mut(), parents, children_per_parent);
        let children_h = app
            .world()
            .resource::<ModRegistry>()
            .get_by_name(BAM_CHILDREN_NAME)
            .expect("AbiModPlugin pre-registers Children")
            .handle;
        ctx.children_h = children_h;
        let reg = app.world().resource::<ModRegistry>().clone();
        let spec = BamQuerySpecOwned::new(vec![BamTermDesc::new(children_h, AccessMode::READ)]);
        let ptr = (&mut *ctx as *mut ChildrenBenchCtx).cast::<c_void>();
        app.add_systems(
            Update,
            GuestSystem::new(
                "bench_entity_children",
                vec![spec],
                c_abi_entity_children_cb,
                ptr,
                reg,
            ),
        );
        measure_fn(
            || timed_update(&mut app),
            cfg.warmup,
            cfg.rounds,
            total_edges,
        )
    };
    assert_eq!(ctx.edges_visited, total_edges);
    black_box(ctx.checksum);
    print_row(
        "C-ABI host_sys_entity_children (&[FfiEntity])",
        cabi_stats,
        "edge",
    );
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
    if cfg.matches("query_filters") {
        bench_query_filters(&cfg);
    }
    if cfg.matches("batch_commands") {
        bench_batch_commands(&cfg);
    }
    if cfg.matches("summary_tick") {
        bench_summary_tick(&cfg);
    }
    if cfg.matches("entity_children") {
        bench_entity_children(&cfg);
    }

    println!("\nCanonical benchmark suite completed cleanly.");
}
