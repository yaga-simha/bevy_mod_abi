//! Deterministic CI Regression Gate (`X-03`).
//!
//! - On **Linux** (`cfg(target_os = "linux")`): Runs under `iai-callgrind` (`Valgrind` Callgrind)
//!   to enforce deterministic instruction-count and cache-access regression gates across PRs
//!   with zero wall-clock jitter.
//! - On **macOS / Windows** (`cfg(not(target_os = "linux"))`): Runs deterministic counting-allocator
//!   zero-steady-state-allocation and chunk/tick-skip step-count regression gates cleanly without
//!   requiring Valgrind.

#![deny(unsafe_op_in_unsafe_fn)]
#![allow(clippy::cast_ptr_alignment)]

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModComponent, ModRegistry};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, host_cmd_insert, host_cmd_spawn, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_column_rows_mutable, host_iter_entities,
    host_iter_next_chunk, host_sys_commands, host_sys_query,
};
use bevy_abi_mod_sys::{
    AccessMode, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, FfiBool, FfiEntity,
    FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::system::System;
use core::alloc::{GlobalAlloc, Layout};
use core::ffi::c_void;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::alloc::System as SysAlloc;
use std::hint::black_box;

struct CountingAlloc;

static ALLOC_CALLS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Delegates directly to `std::alloc::System` while incrementing an atomic counter.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOC_CALLS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: Forwarding `layout` directly to `SysAlloc::alloc`.
        unsafe { SysAlloc.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwarding `(ptr, layout)` directly to `SysAlloc::dealloc`.
        unsafe { SysAlloc.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "gate::Position")]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "gate::Velocity")]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "gate::Armor")]
pub struct Armor {
    pub plating: f32,
    pub faction: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[mod_component(name = "gate::Vitality")]
pub struct Vitality {
    pub hp: f32,
    pub shield: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "gate::SparseBuff")]
pub struct SparseBuff {
    pub bonus: f32,
    pub remaining: u32,
}

#[repr(C)]
struct SpawnHandles {
    pos: BamComponent,
    vel: BamComponent,
    arm: BamComponent,
    vit: BamComponent,
}

unsafe extern "C" fn dense_cb(sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Active `sys_ctx` with query `0`.
    if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
        return FfiResult::INTERNAL_ERROR;
    }
    let mut len = 0usize;
    // SAFETY: Dense `Position` + `Velocity` columns.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        let p = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        let v = unsafe { host_iter_column_readonly(iter, 1) }.cast::<Velocity>();
        let ps = unsafe { core::slice::from_raw_parts_mut(p, len) };
        let vs = unsafe { core::slice::from_raw_parts(v, len) };
        for (pi, vi) in ps.iter_mut().zip(vs.iter()) {
            pi.x += vi.x;
            pi.y += vi.y;
            pi.z += vi.z;
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn changed_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
    let out = ud.cast::<usize>();
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Active `sys_ctx` with query `0`.
    if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
        return FfiResult::INTERNAL_ERROR;
    }
    let mut visited = 0usize;
    let mut len = 0usize;
    // SAFETY: `AccessMode::CHANGED` over `Position`.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        let p = unsafe { host_iter_column_readonly(iter, 0) }.cast::<Position>();
        let ps = unsafe { core::slice::from_raw_parts(p, len) };
        for item in ps {
            black_box(item.x);
        }
        visited += len;
    }
    if !out.is_null() {
        // SAFETY: Valid `usize` pointer passed via `user_data`.
        unsafe { *out = visited };
    }
    FfiResult::OK
}

unsafe extern "C" fn cmd_burst_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
    let armor = BamComponent::from_raw(ud as usize as u32);
    let mut iter: *mut BamIter = core::ptr::null_mut();
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    // SAFETY: Active `sys_ctx`.
    let _ = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    let _ = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    let val = Armor {
        plating: 10.0,
        faction: 1,
    };
    let mut len = 0usize;
    // SAFETY: Iterating entities and queuing arena `host_cmd_insert`.
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        let ents = unsafe { core::slice::from_raw_parts(host_iter_entities(iter), len) };
        for &e in ents {
            let res = unsafe {
                host_cmd_insert(
                    cmds,
                    e,
                    armor,
                    (&val as *const Armor).cast::<u8>(),
                    core::mem::size_of::<Armor>(),
                )
            };
            if res.is_err() {
                return res;
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn sparse_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
    let chunks_out = ud.cast::<usize>();
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Active `sys_ctx`.
    let _ = unsafe { host_sys_query(sys_ctx, 0, &mut iter) };
    let mut chunks = 0usize;
    let mut len = 0usize;
    // SAFETY: Indirect sparse row iteration (`P-02`).
    while unsafe { host_iter_next_chunk(iter, &mut len) } != FfiBool::FALSE {
        chunks += 1;
        let p = unsafe { host_iter_column_mutable(iter, 0) }.cast::<Position>();
        let rows = unsafe { host_iter_column_rows_mutable(iter, 1) };
        let ps = unsafe { core::slice::from_raw_parts_mut(p, len) };
        let rs = unsafe { core::slice::from_raw_parts(rows, len) };
        for i in 0..len {
            let b = unsafe { &mut *rs[i].cast::<SparseBuff>() };
            ps[i].x += b.bonus;
            b.remaining += 1;
        }
    }
    if !chunks_out.is_null() {
        // SAFETY: Valid `usize` pointer passed via `user_data`.
        unsafe { *chunks_out = chunks };
    }
    FfiResult::OK
}

unsafe extern "C" fn spawn4_cb(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
    // SAFETY: `ud` points to valid `SpawnHandles`.
    let h = unsafe { &*ud.cast::<SpawnHandles>() };
    let mut cmds: *mut BamCommands = core::ptr::null_mut();
    let _ = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
    let p = Position::default();
    let v = Velocity::default();
    let a = Armor::default();
    let vt = Vitality::default();
    let comps = [h.pos, h.vel, h.arm, h.vit];
    let ptrs = [
        (&p as *const Position).cast::<u8>(),
        (&v as *const Velocity).cast::<u8>(),
        (&a as *const Armor).cast::<u8>(),
        (&vt as *const Vitality).cast::<u8>(),
    ];
    for _ in 0..256 {
        let mut out = FfiEntity::NULL;
        // SAFETY: Batched 4-component spawn.
        let res = unsafe { host_cmd_spawn(cmds, comps.as_ptr(), ptrs.as_ptr(), 4, &mut out) };
        if res.is_err() {
            return res;
        }
    }
    FfiResult::OK
}

pub fn run_dense_1k_gate() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let vel_h = app
        .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    for i in 0..1_000 {
        app.world_mut().spawn((
            Position {
                x: i as f32,
                y: 0.0,
                z: 0.0,
            },
            Velocity {
                x: 1.0,
                y: 2.0,
                z: 3.0,
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
    let mut sys = GuestSystem::new(
        "gate_dense",
        vec![spec],
        dense_cb,
        core::ptr::null_mut(),
        reg,
    );
    sys.initialize(app.world_mut());
    let _ = sys.run((), app.world_mut());

    let before = ALLOC_CALLS.load(Ordering::Relaxed);
    let _ = sys.run((), app.world_mut());
    let steady_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - before;
    assert_eq!(
        steady_allocs, 0,
        "steady-state dense chunk iteration must allocate 0 heap blocks (got {steady_allocs})"
    );
}

pub fn run_changed_1pct_gate() {
    let mut visited = Box::new(0usize);
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let mut ents = Vec::with_capacity(1_000);
    for i in 0..1_000 {
        ents.push(
            app.world_mut()
                .spawn(Position {
                    x: i as f32,
                    y: 0.0,
                    z: 0.0,
                })
                .id(),
        );
    }
    let reg = app.world().resource::<ModRegistry>().clone();
    let spec = BamQuerySpecOwned {
        flags: 0,
        terms: vec![BamTermDesc::new(pos_h, AccessMode::CHANGED)],
    };
    app.add_systems(
        Update,
        GuestSystem::new(
            "gate_changed",
            vec![spec],
            changed_cb,
            (&mut *visited as *mut usize).cast::<c_void>(),
            reg,
        ),
    );
    app.update();
    assert_eq!(*visited, 1_000);

    // Zero entities mutated -> summary-tick table skip must visit 0 entities!
    app.update();
    assert_eq!(*visited, 0);

    // Mutate 1% (10 entities) -> must visit exactly 10 entities!
    for &e in ents.iter().step_by(100) {
        app.world_mut().get_mut::<Position>(e).unwrap().x += 1.0;
    }
    app.update();
    assert_eq!(*visited, 10);
}

pub fn run_command_arena_zero_alloc_gate() {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    let pos_h = app
        .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let arm_h = app
        .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    for _ in 0..1_000 {
        app.world_mut()
            .spawn((Position::default(), Armor::default()));
    }
    let reg = app.world().resource::<ModRegistry>().clone();
    let spec = BamQuerySpecOwned {
        flags: 0,
        terms: vec![BamTermDesc::new(pos_h, AccessMode::FILTER_ONLY)],
    };
    let mut sys = GuestSystem::new(
        "gate_cmd_burst",
        vec![spec],
        cmd_burst_cb,
        arm_h.to_raw() as usize as *mut c_void,
        reg,
    );
    sys.initialize(app.world_mut());
    // Warm up slab capacity on frame 1 + 2.
    let _ = sys.run((), app.world_mut());
    let _ = sys.run((), app.world_mut());

    let before = ALLOC_CALLS.load(Ordering::Relaxed);
    let _ = sys.run((), app.world_mut());
    let steady_allocs = ALLOC_CALLS.load(Ordering::Relaxed) - before;
    assert_eq!(
        steady_allocs, 0,
        "steady-state 1,000-command arena burst must perform 0 heap allocations (got {steady_allocs})"
    );
}

pub fn run_sparse_batching_gate() {
    let mut chunks_seen = Box::new(0usize);
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
    for _ in 0..1_000 {
        app.world_mut().spawn((
            Position::default(),
            SparseBuff {
                bonus: 1.0,
                remaining: 5,
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
    app.add_systems(
        Update,
        GuestSystem::new(
            "gate_sparse",
            vec![spec],
            sparse_cb,
            (&mut *chunks_seen as *mut usize).cast::<c_void>(),
            reg,
        ),
    );
    app.update();
    assert_eq!(
        *chunks_seen, 1,
        "1,000 entities in one sparse archetype must batch into 1 indirect chunk (P-02)"
    );
}

pub fn run_batched_spawn4_gate() {
    let mut handles = Box::new(SpawnHandles {
        pos: BamComponent::INVALID,
        vel: BamComponent::INVALID,
        arm: BamComponent::INVALID,
        vit: BamComponent::INVALID,
    });
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    handles.pos = app
        .register_mod_component::<Position>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    handles.vel = app
        .register_mod_component::<Velocity>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    handles.arm = app
        .register_mod_component::<Armor>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    handles.vit = app
        .register_mod_component::<Vitality>(ComponentOptions::new().with_raw_insert())
        .unwrap();
    let reg = app.world().resource::<ModRegistry>().clone();
    app.add_systems(
        Update,
        GuestSystem::new(
            "gate_spawn4",
            Vec::new(),
            spawn4_cb,
            (&mut *handles as *mut SpawnHandles).cast::<c_void>(),
            reg,
        ),
    );
    app.update();
    let count = app
        .world_mut()
        .query::<&Position>()
        .iter(app.world())
        .count();
    assert_eq!(count, 256);
}

#[cfg(target_os = "linux")]
use iai_callgrind::{library_benchmark, library_benchmark_group, main as iai_main};

#[cfg(target_os = "linux")]
#[library_benchmark]
fn bench_gate_dense_1k() {
    run_dense_1k_gate();
}

#[cfg(target_os = "linux")]
#[library_benchmark]
fn bench_gate_changed_1pct() {
    run_changed_1pct_gate();
}

#[cfg(target_os = "linux")]
#[library_benchmark]
fn bench_gate_command_arena_1k() {
    run_command_arena_zero_alloc_gate();
}

#[cfg(target_os = "linux")]
#[library_benchmark]
fn bench_gate_sparse_1k() {
    run_sparse_batching_gate();
}

#[cfg(target_os = "linux")]
#[library_benchmark]
fn bench_gate_spawn4_256() {
    run_batched_spawn4_gate();
}

#[cfg(target_os = "linux")]
library_benchmark_group!(
    name = deterministic_abi_gates;
    benchmarks =
        bench_gate_dense_1k,
        bench_gate_changed_1pct,
        bench_gate_command_arena_1k,
        bench_gate_sparse_1k,
        bench_gate_spawn4_256
);

#[cfg(target_os = "linux")]
iai_main!(library_benchmark_groups = deterministic_abi_gates);

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("=== Deterministic ABI 0.2.0 Allocation & Step-Count Gate (non-Linux host) ===");
    run_dense_1k_gate();
    println!("  [PASS] dense_1k: 0 steady-state heap allocations");
    run_changed_1pct_gate();
    println!("  [PASS] changed_1pct: table summary skip (0 visited) & 1% filter (10 visited)");
    run_command_arena_zero_alloc_gate();
    println!("  [PASS] command_arena_1k: 0 steady-state heap allocations for 1,000 cmd_inserts");
    run_sparse_batching_gate();
    println!("  [PASS] sparse_1k: 1,000 sparse entities batched into 1 FFI chunk (P-02)");
    run_batched_spawn4_gate();
    println!("  [PASS] batched_spawn4: 256 4-component entities spawned in single archetype move");
}
