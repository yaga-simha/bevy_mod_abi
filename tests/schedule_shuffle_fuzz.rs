#![forbid(unsafe_code)]

//! Phase 4 Item #22 — Bevy `0.20.1` `ScheduleBuildSettings::shuffle_seed` Property Fuzzing Suite.
//!
//! Constructs a deterministic 5-mod ABI `0.3.0` DAG compiled from five `#![forbid(unsafe_code)]`
//! native Rust guest `.cdylib` mods:
//! 1. `mod_a_seq`: strict `BAM_SCHEDULE_SET_FLAG_SEQUENCE` (`.chain()`), intra-mod
//!    `before_sets` + `after_sets` bridge ordering, and global + entity observer event triggers.
//! 2. `mod_b_weak`: `BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK` (`.chain_weak()`), cross-mod
//!    `after_sets` dependency on `mod_a_seq`, plus global, entity-targeted, and component
//!    lifecycle (`observe_add`) guest observers.
//! 3. `mod_c_parallel`: `BAM_SCHEDULE_SET_FLAG_PARALLEL` container running 3 disjoint parallel
//!    branches (`BranchA`, `BranchB`, `BranchC`) after `mod_b_weak`.
//! 4. `mod_d_relations`: `register_relation` (`EXCLUSIVE | ACYCLIC | CLEANUP_REMOVE`,
//!    `SYMMETRIC | CLEANUP_REMOVE`, `ACYCLIC | CASCADE_DELETE`), `cmd_insert_relation` (including
//!    exclusive eviction and cycle rejection), and `cmd_remove_relation`.
//! 5. `mod_e_reduce`: final `SEQUENCE` reduction folding parallel branches + observer badges into
//!    `UnitScore`, appending a cross-mod symmetric relation edge, and recording a canonical
//!    order-independent checksum into `PipelineTrace`.
//!
//! Executes **256 deterministic `ScheduleBuildSettings { shuffle_seed: Some(seed), .. }`
//! permutations** under `ambiguity_detection: LogLevel::Error` and asserts **bit-exact identical
//! world-state hashes** across all 256 seeds, plus verifies that Bevy's shuffler genuinely
//! permutes parallel system execution order across seeds.

use bevy_abi_mod_host::prelude::*;
use bevy_abi_mod_host_sys::registry::catalog::{
    ModRelationGraph, RelationFlushPolicy, ensure_relation_graph_in_world,
};
use bevy_app::{App, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::event::{EntityEvent, Event};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{
    LogLevel, MultiThreadedExecutor, ScheduleBuildSettings, Schedules, SingleThreadedExecutor,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

const NUM_SHUFFLE_SEEDS: usize = 256;
const NUM_UNITS: usize = 8;

// ============================================================================
// Host-Registered Shared Components, Resources, & Events (Zero Implicit Padding)
// ============================================================================

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::UnitScore")]
struct UnitScore {
    score: i64,
    id: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchA")]
struct BranchA {
    val: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchB")]
struct BranchB {
    val: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchC")]
struct BranchC {
    val: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::ObservedBadge")]
struct ObservedBadge {
    hits: u32,
    last_delta: i32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::ObserverAck")]
struct ObserverAck {
    code: u32,
    entity_lo: u32,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::PipelineTrace")]
struct PipelineTrace {
    acc: i64,
    steps: [u32; 32],
    len: u32,
    observer_hits: u32,
}

impl Default for PipelineTrace {
    fn default() -> Self {
        Self {
            acc: 17,
            steps: [0u32; 32],
            len: 0,
            observer_hits: 0,
        }
    }
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::SeedAnchor")]
struct SeedAnchor {
    entities: [u64; 8],
    count: u32,
    tick: u32,
}

#[repr(C)]
#[derive(Event, Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::DagPulseEvent")]
struct DagPulseEvent {
    tag: u32,
    delta: i32,
}

#[repr(C)]
#[derive(EntityEvent, Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::UnitPulseEvent")]
struct UnitPulseEvent {
    #[event_target]
    entity: TargetEntity,
    delta: i32,
    tag: u32,
}

// ============================================================================
// Five-Mod `#![forbid(unsafe_code)]` Guest Workspace Builder
// ============================================================================

struct BuiltDagMods {
    root_dir: PathBuf,
    target_dir: PathBuf,
    mod_a: PathBuf,
    mod_b: PathBuf,
    mod_c: PathBuf,
    mod_d: PathBuf,
    mod_e: PathBuf,
}

impl Drop for BuiltDagMods {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root_dir);
        let _ = fs::remove_dir_all(&self.target_dir);
    }
}

fn sanitized_cargo_command() -> Command {
    let cargo = std::env::var_os("CARGO")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    for var in [
        "RUSTFLAGS",
        "CARGO_ENCODED_RUSTFLAGS",
        "LLVM_PROFILE_FILE",
        "CARGO_LLVM_COV",
        "CARGO_LLVM_COV_TARGET_DIR",
        "CARGO_LLVM_COV_BUILD_DIR",
        "RUSTC_WRAPPER",
        "CARGO_PROFILE_RELEASE_PANIC",
        "CARGO_PROFILE_RELEASE_OPT_LEVEL",
        "CARGO_PROFILE_DEV_PANIC",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_RUSTFLAGS",
        "__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES",
    ] {
        cmd.env_remove(var);
    }
    cmd
}

const SHARED_TYPES_RS: &str = r#"
#![allow(dead_code)]
use core::ffi::CStr;
use bevy_abi_mod_guest::prelude::*;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::UnitScore")]
pub struct UnitScore {
    pub score: i64,
    pub id: u32,
    pub flags: u32,
}
impl GuestComponent for UnitScore {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchA")]
pub struct BranchA {
    pub val: i64,
}
impl GuestComponent for BranchA {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchB")]
pub struct BranchB {
    pub val: i64,
}
impl GuestComponent for BranchB {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::BranchC")]
pub struct BranchC {
    pub val: i64,
}
impl GuestComponent for BranchC {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::ObservedBadge")]
pub struct ObservedBadge {
    pub hits: u32,
    pub last_delta: i32,
}
impl GuestComponent for ObservedBadge {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::ObserverAck")]
pub struct ObserverAck {
    pub code: u32,
    pub entity_lo: u32,
}
impl GuestComponent for ObserverAck {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::PipelineTrace")]
pub struct PipelineTrace {
    pub acc: i64,
    pub steps: [u32; 32],
    pub len: u32,
    pub observer_hits: u32,
}
impl GuestComponent for PipelineTrace {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::SeedAnchor")]
pub struct SeedAnchor {
    pub entities: [u64; 8],
    pub count: u32,
    pub tick: u32,
}
impl GuestComponent for SeedAnchor {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "dag::DagPulseEvent")]
pub struct DagPulseEvent {
    pub tag: u32,
    pub delta: i32,
}
impl GuestComponent for DagPulseEvent {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct UnitPulseWire {
    pub entity: FfiEntity,
    pub delta: i32,
    pub tag: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct EdgeWeight {
    pub weight: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, AbiPod)]
pub struct AllyLink {
    pub strength: u32,
}

#[inline]
pub fn record_step(trace: &mut PipelineTrace, tag: u32) {
    let idx = trace.len as usize;
    if idx < trace.steps.len() {
        trace.steps[idx] = tag;
        trace.len += 1;
    }
    trace.acc = trace.acc.wrapping_mul(31).wrapping_add(i64::from(tag));
}
"#;

const MOD_A_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        // Set 0: lead SEQUENCE (.chain())
        let set_lead = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_a_lead")
                .schedule(ScheduleId::UPDATE)
                .sequence(),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };
        // Set 1: follow SEQUENCE (.after(set_lead))
        let set_follow = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_a_follow")
                .schedule(ScheduleId::UPDATE)
                .sequence()
                .after(set_lead),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };
        // Set 2: bridge SEQUENCE wired with BOTH `after_sets` (`set_lead`) and `before_sets` (`set_follow`)!
        let set_bridge = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_a_bridge")
                .schedule(ScheduleId::UPDATE)
                .sequence()
                .after(set_lead)
                .before(set_follow),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let unit_pulse_h = match app.resolve_component_named(Some(c"dag::UnitPulseEvent"), None) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let s1 = match app
            .system(c"a_init_1")
            .query(|q| q.write::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 101);
                }
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                        for u in scores.iter_mut() {
                            u.score = u.score.wrapping_add(5);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let s2 = match app
            .system(c"a_init_2")
            .query(|q| q.write::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 102);
                }
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                        for u in scores.iter_mut() {
                            // Non-commutative with `a_init_1` (+5 vs *2)
                            u.score = u.score.wrapping_mul(2);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let s_bridge = match app
            .system(c"a_bridge")
            .query(|q| q.write::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 103);
                }
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                        for u in scores.iter_mut() {
                            u.score = u.score.wrapping_add(i64::from(u.id) * 3);
                            u.flags |= 1;
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let s_tail = match app
            .system(c"a_tail")
            .resource_write::<PipelineTrace>()
            .resource_read::<SeedAnchor>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(move |ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 104);
                }
                let anchor = match ctx.resource::<SeedAnchor>(1) {
                    Some(a) => *a,
                    None => return,
                };
                let cmds = ctx.commands();
                let _ = cmds.trigger(&DagPulseEvent { tag: 7, delta: 19 });
                let e0 = FfiEntity::from_bits(anchor.entities[0]);
                let e3 = FfiEntity::from_bits(anchor.entities[3]);
                let _ = cmds.trigger_handle(
                    unit_pulse_h,
                    e0,
                    &UnitPulseWire {
                        entity: e0,
                        delta: 23,
                        tag: 1,
                    },
                );
                let _ = cmds.trigger_handle(
                    unit_pulse_h,
                    e3,
                    &UnitPulseWire {
                        entity: e3,
                        delta: 41,
                        tag: 2,
                    },
                );
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let rc = app.mount_system(s1, ScheduleId::UPDATE, set_lead);
        if rc.is_err() { return rc; }
        let rc = app.mount_system(s2, ScheduleId::UPDATE, set_lead);
        if rc.is_err() { return rc; }
        let rc = app.mount_system(s_bridge, ScheduleId::UPDATE, set_bridge);
        if rc.is_err() { return rc; }
        app.mount_system(s_tail, ScheduleId::UPDATE, set_follow)
    }
}

bevy_abi_mod_guest::export_mod!(name: c"mod_a_seq", version: c"0.3.0", logic::build);
"#;

const MOD_B_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let unit_pulse_h = match app.resolve_component_named(Some(c"dag::UnitPulseEvent"), None) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let badge_h = match app.resolve_component::<ObservedBadge>() {
            Ok(h) => h,
            Err(e) => return e,
        };
        let ack_h = match app.resolve_component::<ObserverAck>() {
            Ok(h) => h,
            Err(e) => return e,
        };

        // 1. Global observer on `DagPulseEvent`.
        if let Err(e) = app.observe::<DagPulseEvent, _>("obs_dag_pulse", |_ev, _cmds| {}) {
            return e;
        }

        // 2. Entity-targeted observer on `UnitPulseEvent` -> inserts `ObservedBadge` on target.
        if let Err(e) = app.observe_handle::<UnitPulseWire, _>(
            "obs_unit_pulse",
            unit_pulse_h,
            FfiEntity::NULL,
            move |target, ev, cmds| {
                if !target.is_null() {
                    let _ = cmds.insert(
                        target,
                        badge_h,
                        &ObservedBadge {
                            hits: ev.tag,
                            last_delta: ev.delta,
                        },
                    );
                }
            },
        ) {
            return e;
        }

        // 3. Component lifecycle observer on `ADD` of `ObservedBadge` -> inserts `ObserverAck`.
        if let Err(e) = app.observe_add::<ObservedBadge, _>("obs_badge_add", move |target, cmds| {
            if !target.is_null() {
                let _ = cmds.insert(
                    target,
                    ack_h,
                    &ObserverAck {
                        code: 0xA55A,
                        entity_lo: (target.to_bits() & 0xFFFF_FFFF) as u32,
                    },
                );
            }
        }) {
            return e;
        }

        // Set 3: SEQUENCE_WEAK (.chain_weak()) strictly after `mod_a_seq::set_a_follow` (handle 1).
        let set_weak = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_b_weak")
                .schedule(ScheduleId::UPDATE)
                .sequence_weak()
                .after(BamSystemSet::from_raw(1)),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let b1 = match app
            .system(c"b_step_1")
            .query(|q| q.write::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 201);
                }
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                        for u in scores.iter_mut() {
                            u.score ^= i64::from(u.id).wrapping_mul(7);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let b_mid = match app
            .system(c"b_independent_read")
            .query(|q| q.read::<UnitScore>().write::<BranchA>())
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some((scores, branch_a)) =
                        chunk.component_read_write::<UnitScore, BranchA>(0, 1)
                    {
                        for (u, a) in scores.iter().zip(branch_a.iter_mut()) {
                            a.val = u.score.wrapping_add(11);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let b2 = match app
            .system(c"b_step_2")
            .query(|q| q.write::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 202);
                }
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                        for u in scores.iter_mut() {
                            u.score = u.score.wrapping_add(17);
                            u.flags |= 2;
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let rc = app.mount_system(b1, ScheduleId::UPDATE, set_weak);
        if rc.is_err() { return rc; }
        let rc = app.mount_system(b_mid, ScheduleId::UPDATE, set_weak);
        if rc.is_err() { return rc; }
        app.mount_system(b2, ScheduleId::UPDATE, set_weak)
    }
}

bevy_abi_mod_guest::export_mod!(name: c"mod_b_weak", version: c"0.3.0", logic::build);
"#;

const MOD_C_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        // Set 4: PARALLEL set running after `mod_b_weak::set_b_weak` (handle 3).
        // The 3 mounted systems access strictly disjoint mutable components (`BranchA`, `BranchB`,
        // `BranchC`) with shared read-only `UnitScore`, so Bevy's `shuffle_seed` freely shuffles
        // their topological order with zero ambiguities.
        let set_par = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_c_par")
                .schedule(ScheduleId::UPDATE)
                .parallel()
                .after(BamSystemSet::from_raw(3)),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let pa = match app
            .system(c"c_par_a")
            .query(|q| q.read::<UnitScore>().write::<BranchA>())
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some((scores, branch_a)) =
                        chunk.component_read_write::<UnitScore, BranchA>(0, 1)
                    {
                        for (u, a) in scores.iter().zip(branch_a.iter_mut()) {
                            a.val = a.val.wrapping_mul(3).wrapping_add(u.score).wrapping_add(1);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let pb = match app
            .system(c"c_par_b")
            .query(|q| q.read::<UnitScore>().write::<BranchB>())
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some((scores, branch_b)) =
                        chunk.component_read_write::<UnitScore, BranchB>(0, 1)
                    {
                        for (u, b) in scores.iter().zip(branch_b.iter_mut()) {
                            b.val = u.score.wrapping_mul(5).wrapping_sub(i64::from(u.id));
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let pc = match app
            .system(c"c_par_c")
            .query(|q| q.read::<UnitScore>().write::<BranchC>())
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                let iter = ctx.iter();
                while let Some(mut chunk) = iter.next_chunk_view() {
                    if let Some((scores, branch_c)) =
                        chunk.component_read_write::<UnitScore, BranchC>(0, 1)
                    {
                        for (u, c) in scores.iter().zip(branch_c.iter_mut()) {
                            c.val = (u.score ^ 0x55).wrapping_add(9);
                        }
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let rc = app.mount_system(pa, ScheduleId::UPDATE, set_par);
        if rc.is_err() { return rc; }
        let rc = app.mount_system(pb, ScheduleId::UPDATE, set_par);
        if rc.is_err() { return rc; }
        app.mount_system(pc, ScheduleId::UPDATE, set_par)
    }
}

bevy_abi_mod_guest::export_mod!(name: c"mod_c_parallel", version: c"0.3.0", logic::build);
"#;

const MOD_D_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let rel_parent = match app.register_relation(
            RelationDescBuilder::new("DagParent")
                .exclusive()
                .acyclic()
                .cleanup_remove()
                .with_payload::<EdgeWeight>(),
        ) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let rel_ally = match app.register_relation(
            RelationDescBuilder::new("DagAlly")
                .symmetric()
                .cleanup_remove()
                .with_payload::<AllyLink>(),
        ) {
            Ok(h) => h,
            Err(e) => return e,
        };
        let rel_sub = match app.register_relation(
            RelationDescBuilder::new("DagSub")
                .acyclic()
                .cascade_delete(),
        ) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let unit_pulse_h = match app.resolve_component_named(Some(c"dag::UnitPulseEvent"), None) {
            Ok(h) => h,
            Err(e) => return e,
        };

        // Set 5: build relations after `mod_c_parallel::set_c_par` (handle 4).
        let set_rel_build = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_d_rel_build")
                .schedule(ScheduleId::UPDATE)
                .sequence()
                .after(BamSystemSet::from_raw(4)),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        // Set 6: prune relations after `set_rel_build`.
        let set_rel_prune = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_d_rel_prune")
                .schedule(ScheduleId::UPDATE)
                .sequence()
                .after(set_rel_build),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let d1 = match app
            .system(c"d_insert_edges")
            .resource_write::<PipelineTrace>()
            .resource_read::<SeedAnchor>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(move |ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 401);
                }
                let anchor = match ctx.resource::<SeedAnchor>(1) {
                    Some(a) => *a,
                    None => return,
                };
                let e = |i: usize| FfiEntity::from_bits(anchor.entities[i]);
                let cmds = ctx.commands();

                let _ = cmds.insert_relation_with_payload(e(1), rel_parent, e(0), &EdgeWeight { weight: 10 });
                let _ = cmds.insert_relation_with_payload(e(2), rel_parent, e(1), &EdgeWeight { weight: 20 });
                let _ = cmds.insert_relation_with_payload(e(3), rel_parent, e(1), &EdgeWeight { weight: 30 });
                let _ = cmds.insert_relation_with_payload(e(4), rel_parent, e(2), &EdgeWeight { weight: 40 });
                // EXCLUSIVE eviction: reparents e(4) -> e(3) (evicting e(4) -> e(2))
                let _ = cmds.insert_relation_with_payload(e(4), rel_parent, e(3), &EdgeWeight { weight: 45 });
                // Intentional cycle attempt: e(0) -> e(3) closes 0 -> 3 -> 1 -> 0 -> rejected by ACYCLIC!
                let _ = cmds.insert_relation_with_payload(e(0), rel_parent, e(3), &EdgeWeight { weight: 999 });

                // SYMMETRIC ally edges
                let _ = cmds.insert_relation_with_payload(e(0), rel_ally, e(5), &AllyLink { strength: 7 });
                let _ = cmds.insert_relation_with_payload(e(2), rel_ally, e(6), &AllyLink { strength: 13 });
                let _ = cmds.insert_relation_with_payload(e(3), rel_ally, e(7), &AllyLink { strength: 19 });

                // Marker ACYCLIC + CASCADE_DELETE edges
                let _ = cmds.insert_relation(e(6), rel_sub, e(5));
                let _ = cmds.insert_relation(e(7), rel_sub, e(6));
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let d2 = match app
            .system(c"d_trigger_rel_pulse")
            .resource_write::<PipelineTrace>()
            .resource_read::<SeedAnchor>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(move |ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 402);
                }
                let anchor = match ctx.resource::<SeedAnchor>(1) {
                    Some(a) => *a,
                    None => return,
                };
                let e = |i: usize| FfiEntity::from_bits(anchor.entities[i]);
                let cmds = ctx.commands();
                // Deferred cycle attempt after `d_insert_edges` flushed `e1->e0` and `e3->e1`:
                // rejected at flush time with `FfiResult::CYCLE_DETECTED` into `ModRelationGraph::flush_errors`.
                let _ = cmds.insert_relation_with_payload(
                    e(0),
                    rel_parent,
                    e(3),
                    &EdgeWeight { weight: 888 },
                );
                let _ = cmds.trigger_handle(
                    unit_pulse_h,
                    e(2),
                    &UnitPulseWire {
                        entity: e(2),
                        delta: 59,
                        tag: 3,
                    },
                );
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let d3 = match app
            .system(c"d_remove_edges")
            .resource_write::<PipelineTrace>()
            .resource_read::<SeedAnchor>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(move |ctx| {
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 403);
                }
                let anchor = match ctx.resource::<SeedAnchor>(1) {
                    Some(a) => *a,
                    None => return,
                };
                let e = |i: usize| FfiEntity::from_bits(anchor.entities[i]);
                let cmds = ctx.commands();
                // Symmetric edge removal clears both (e2 -> e6) and (e6 -> e2).
                let _ = cmds.remove_relation(e(2), rel_ally, e(6));
                let _ = cmds.remove_relation(e(3), rel_parent, e(1));
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let rc = app.mount_system(d1, ScheduleId::UPDATE, set_rel_build);
        if rc.is_err() { return rc; }
        let rc = app.mount_system(d2, ScheduleId::UPDATE, set_rel_build);
        if rc.is_err() { return rc; }
        app.mount_system(d3, ScheduleId::UPDATE, set_rel_prune)
    }
}

bevy_abi_mod_guest::export_mod!(name: c"mod_d_relations", version: c"0.3.0", logic::build);
"#;

const MOD_E_LIB_RS: &str = r#"
mod shared;

mod logic {
    #![forbid(unsafe_code)]
    use crate::shared::*;
    use bevy_abi_mod_guest::prelude::*;

    pub fn build(app: &mut GuestApp<'_>) -> FfiResult {
        let rel_ally = match app.resolve_component_named(Some(c"mod_d_relations::DagAlly"), None) {
            Ok(h) => h,
            Err(e) => return e,
        };

        // Set 7: final SEQUENCE reduction strictly after `mod_d_relations::set_d_rel_prune` (handle 6).
        let set_final = match app.configure_schedule_set(
            ScheduleSetBuilder::new("set_e_final")
                .schedule(ScheduleId::UPDATE)
                .sequence()
                .after(BamSystemSet::from_raw(6)),
        ) {
            Ok(s) => s,
            Err(e) => return e,
        };

        let e_fold = match app
            .system(c"e_fold_branches")
            .query(|q| {
                q.write::<UnitScore>()
                    .read::<BranchA>()
                    .read::<BranchB>()
                    .read::<BranchC>()
                    .optional_read::<ObservedBadge>()
            })
            .resource_write::<PipelineTrace>()
            .resource_read::<SeedAnchor>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(move |ctx| {
                let mut badge_hits = 0u32;
                {
                    let iter = ctx.iter();
                    while let Some(mut chunk) = iter.next_chunk_view() {
                        let a_vals: Vec<i64> = chunk
                            .component::<BranchA>(1)
                            .map(|s| s.iter().map(|v| v.val).collect())
                            .unwrap_or_default();
                        let b_vals: Vec<i64> = chunk
                            .component::<BranchB>(2)
                            .map(|s| s.iter().map(|v| v.val).collect())
                            .unwrap_or_default();
                        let c_vals: Vec<i64> = chunk
                            .component::<BranchC>(3)
                            .map(|s| s.iter().map(|v| v.val).collect())
                            .unwrap_or_default();
                        let badges: Option<Vec<ObservedBadge>> = chunk
                            .optional_component::<ObservedBadge>(4)
                            .map(|s| s.to_vec());
                        if let Some(scores) = chunk.component_mut::<UnitScore>(0) {
                            for (i, u) in scores.iter_mut().enumerate() {
                                let bonus = badges
                                    .as_ref()
                                    .map_or(0i64, |bs| i64::from(bs[i].last_delta) * 13);
                                if badges.is_some() {
                                    badge_hits += 1;
                                }
                                u.score = u
                                    .score
                                    .wrapping_add(a_vals[i])
                                    .wrapping_add(b_vals[i])
                                    .wrapping_add(c_vals[i])
                                    .wrapping_add(bonus);
                                u.flags |= 4;
                            }
                        }
                    }
                }
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    trace.observer_hits = trace.observer_hits.wrapping_add(badge_hits);
                    record_step(trace, 501);
                }
                if let Some(anchor) = ctx.resource::<SeedAnchor>(1).copied() {
                    let e1 = FfiEntity::from_bits(anchor.entities[1]);
                    let e7 = FfiEntity::from_bits(anchor.entities[7]);
                    let _ = ctx.commands().insert_relation_with_payload(
                        e1,
                        rel_ally,
                        e7,
                        &AllyLink { strength: 42 },
                    );
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let e_fin = match app
            .system(c"e_finalize")
            .query(|q| q.read::<UnitScore>())
            .resource_write::<PipelineTrace>()
            .schedule(ScheduleId::UPDATE)
            .register_handle(|ctx| {
                // Collect indexed by `id` (`0..8`) so reduction is invariant to ECS table row order.
                let mut by_id = [0i64; 8];
                {
                    let iter = ctx.iter();
                    while let Some(chunk) = iter.next_chunk_view() {
                        if let Some(scores) = chunk.component::<UnitScore>(0) {
                            for u in scores {
                                if (u.id as usize) < by_id.len() {
                                    by_id[u.id as usize] = u.score;
                                }
                            }
                        }
                    }
                }
                if let Some(trace) = ctx.resource_mut::<PipelineTrace>(0) {
                    record_step(trace, 502);
                    for s in by_id {
                        trace.acc = trace.acc.wrapping_mul(131).wrapping_add(s);
                    }
                }
            }) {
            Ok(h) => h,
            Err(e) => return e,
        };

        let rc = app.mount_system(e_fold, ScheduleId::UPDATE, set_final);
        if rc.is_err() { return rc; }
        app.mount_system(e_fin, ScheduleId::UPDATE, set_final)
    }
}

bevy_abi_mod_guest::export_mod!(name: c"mod_e_reduce", version: c"0.3.0", logic::build);
"#;

fn build_five_dag_mods() -> BuiltDagMods {
    static NEXT_WORKSPACE_SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = NEXT_WORKSPACE_SEQ.fetch_add(1, Ordering::Relaxed);
    let workspace_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root");
    let guest_path = workspace_root.join("crates").join("bevy_abi_mod_guest");
    let sys_path = workspace_root.join("crates").join("bevy_abi_mod_sys");
    let guest_clean = guest_path
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();
    let sys_clean = sys_path
        .display()
        .to_string()
        .replace('\\', "/")
        .trim_start_matches("//?/")
        .to_owned();

    let root_dir =
        std::env::temp_dir().join(format!("bam_shuffle_dag_mods_{}_{seq}", std::process::id()));
    let target_dir = std::env::temp_dir().join(format!(
        "bam_shuffle_dag_target_{}_{seq}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&root_dir);
    let _ = fs::remove_dir_all(&target_dir);
    fs::create_dir_all(&root_dir).expect("create temp 5-mod DAG workspace");

    let members = [
        ("mod_a_seq", MOD_A_LIB_RS),
        ("mod_b_weak", MOD_B_LIB_RS),
        ("mod_c_parallel", MOD_C_LIB_RS),
        ("mod_d_relations", MOD_D_LIB_RS),
        ("mod_e_reduce", MOD_E_LIB_RS),
    ];

    fs::write(
        root_dir.join("Cargo.toml"),
        r#"[workspace]
resolver = "2"
members = [
    "mod_a_seq",
    "mod_b_weak",
    "mod_c_parallel",
    "mod_d_relations",
    "mod_e_reduce",
]
"#,
    )
    .expect("write temp workspace Cargo.toml");

    let lock_src = workspace_root
        .join("tests")
        .join("asymmetric_rust_mod")
        .join("Cargo.lock");
    if lock_src.is_file() {
        let mut lock_contents =
            fs::read_to_string(&lock_src).expect("read tests/asymmetric_rust_mod/Cargo.lock");
        if !lock_contents.ends_with('\n') {
            lock_contents.push('\n');
        }
        for (crate_name, _) in members {
            lock_contents.push_str(&format!(
                "\n[[package]]\nname = \"{crate_name}\"\nversion = \"0.3.0\"\ndependencies = [\n \"bevy_abi_mod_guest\",\n \"bevy_abi_mod_sys\",\n]\n"
            ));
        }
        fs::write(root_dir.join("Cargo.lock"), lock_contents)
            .expect("write 5-mod DAG workspace Cargo.lock");
    }

    for (crate_name, lib_rs) in members {
        let crate_dir = root_dir.join(crate_name);
        let src_dir = crate_dir.join("src");
        fs::create_dir_all(&src_dir).expect("create member src dir");
        let manifest = format!(
            r#"[package]
name = "{crate_name}"
version = "0.3.0"
edition = "2024"

[lib]
crate-type = ["cdylib"]

[dependencies]
bevy_abi_mod_guest = {{ path = "{guest_clean}", features = ["derive"] }}
bevy_abi_mod_sys = {{ path = "{sys_clean}" }}
"#
        );
        fs::write(crate_dir.join("Cargo.toml"), manifest).expect("write member Cargo.toml");
        fs::write(src_dir.join("shared.rs"), SHARED_TYPES_RS).expect("write shared.rs");
        fs::write(src_dir.join("lib.rs"), lib_rs).expect("write lib.rs");
    }

    let output = sanitized_cargo_command()
        .args(["build", "--workspace", "--release"])
        .current_dir(&root_dir)
        .env("CARGO_TARGET_DIR", &target_dir)
        .output()
        .expect("invoke cargo build for 5-mod shuffle DAG workspace");
    assert!(
        output.status.success(),
        "failed to compile 5-mod shuffle DAG workspace:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let dylib_path = |name: &str| {
        let filename = format!(
            "{}{}{}",
            std::env::consts::DLL_PREFIX,
            name,
            std::env::consts::DLL_SUFFIX
        );
        let built = target_dir.join("release").join(&filename);
        assert!(built.is_file(), "expected dylib at {}", built.display());
        built
    };

    BuiltDagMods {
        mod_a: dylib_path("mod_a_seq"),
        mod_b: dylib_path("mod_b_weak"),
        mod_c: dylib_path("mod_c_parallel"),
        mod_d: dylib_path("mod_d_relations"),
        mod_e: dylib_path("mod_e_reduce"),
        root_dir,
        target_dir,
    }
}

// ============================================================================
// Canonical World-State Hashing (FNV-1a 64-bit)
// ============================================================================

struct Fnv64(u64);

impl Fnv64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0100_0000_01b3;

    fn new() -> Self {
        Self(Self::OFFSET_BASIS)
    }

    fn write_bytes(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(Self::PRIME);
        }
    }

    fn write_u32(&mut self, v: u32) {
        self.write_bytes(&v.to_le_bytes());
    }

    fn write_i32(&mut self, v: i32) {
        self.write_bytes(&v.to_le_bytes());
    }

    fn write_u64(&mut self, v: u64) {
        self.write_bytes(&v.to_le_bytes());
    }

    fn write_i64(&mut self, v: i64) {
        self.write_bytes(&v.to_le_bytes());
    }

    fn finish(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct WorldDigest {
    hash: u64,
    trace_steps: Vec<u32>,
    trace_acc: i64,
    observer_hits: u32,
    parent_edges: usize,
    ally_edges: usize,
    sub_edges: usize,
    cycle_rejections: usize,
}

fn run_seed(mods: &BuiltDagMods, seed: u64, multi_threaded: bool) -> WorldDigest {
    let mut app = App::new();
    app.add_plugins(AbiModPlugin);

    // Configure Bevy `0.20.1` `ScheduleBuildSettings::shuffle_seed` + strict ambiguity errors.
    {
        let mut schedules = app.world_mut().resource_mut::<Schedules>();
        let update_sched = schedules.entry(Update);
        if multi_threaded {
            update_sched.set_executor(MultiThreadedExecutor::new());
        } else {
            update_sched.set_executor(SingleThreadedExecutor::new());
        }
        update_sched.set_build_settings(ScheduleBuildSettings {
            ambiguity_detection: LogLevel::Error,
            shuffle_seed: Some(seed),
            ..Default::default()
        });
    }

    ensure_relation_graph_in_world(app.world_mut());
    app.world_mut()
        .resource_mut::<ModRelationGraph>()
        .set_flush_policy(RelationFlushPolicy::RecordOnly);

    app.register_mod_component::<UnitScore>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register UnitScore");
    app.register_mod_component::<BranchA>(ComponentOptions::new().with_raw_insert().with_default())
        .expect("register BranchA");
    app.register_mod_component::<BranchB>(ComponentOptions::new().with_raw_insert().with_default())
        .expect("register BranchB");
    app.register_mod_component::<BranchC>(ComponentOptions::new().with_raw_insert().with_default())
        .expect("register BranchC");
    app.register_mod_component::<ObservedBadge>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register ObservedBadge");
    app.register_mod_component::<ObserverAck>(
        ComponentOptions::new().with_raw_insert().with_default(),
    )
    .expect("register ObserverAck");
    app.register_mod_resource::<PipelineTrace>()
        .expect("register PipelineTrace");
    app.register_mod_resource::<SeedAnchor>()
        .expect("register SeedAnchor");
    app.register_mod_event::<DagPulseEvent>()
        .expect("register DagPulseEvent");
    app.register_mod_entity_event::<UnitPulseEvent>()
        .expect("register UnitPulseEvent");

    app.insert_resource(PipelineTrace::default());

    let mut entities = [Entity::PLACEHOLDER; NUM_UNITS];
    let mut entity_bits = [0u64; NUM_UNITS];
    for i in 0..NUM_UNITS {
        let e = app
            .world_mut()
            .spawn((
                UnitScore {
                    score: ((i as i64) + 1) * 10,
                    id: i as u32,
                    flags: 0,
                },
                BranchA::default(),
                BranchB::default(),
                BranchC::default(),
            ))
            .id();
        entities[i] = e;
        entity_bits[i] = e.to_bits();
    }
    app.insert_resource(SeedAnchor {
        entities: entity_bits,
        count: NUM_UNITS as u32,
        tick: 0,
    });

    let opts = LoadOptions::new()
        .with_shadow_copy(false)
        .with_warn_on_resident_image(false)
        .strict();
    app.load_mod_with(&mods.mod_a, opts.clone())
        .expect("load mod_a_seq");
    app.load_mod_with(&mods.mod_b, opts.clone())
        .expect("load mod_b_weak");
    app.load_mod_with(&mods.mod_c, opts.clone())
        .expect("load mod_c_parallel");
    app.load_mod_with(&mods.mod_d, opts.clone())
        .expect("load mod_d_relations");
    app.load_mod_with(&mods.mod_e, opts)
        .expect("load mod_e_reduce");

    // Verify `Update` schedule builds cleanly with zero ambiguities under `shuffle_seed: Some(seed)`.
    let mut sched = app
        .world_mut()
        .resource_mut::<Schedules>()
        .remove_temporarily(Update)
        .expect("Update schedule present");
    sched
        .initialize(app.world_mut())
        .unwrap_or_else(|err| panic!("seed {seed} failed schedule build: {err:?}"));
    app.world_mut().resource_mut::<Schedules>().reinsert(sched);

    // Execute two full `Update` ticks so deferred observers & relation mutations feed into tick 2
    // without wasting time initializing unrelated built-in schedules (`First`, `PostUpdate`, etc.)
    // 256 times in unoptimized debug builds.
    app.world_mut().run_schedule(Update);
    app.world_mut().resource_mut::<SeedAnchor>().tick = 1;
    app.world_mut().run_schedule(Update);

    let world = app.world();
    let trace = *world.resource::<PipelineTrace>();
    let trace_steps = trace.steps[..trace.len as usize].to_vec();

    let reg = world.resource::<ModRegistry>();
    let rel_parent = reg
        .get_by_name("mod_d_relations::DagParent")
        .expect("DagParent registered")
        .handle;
    let rel_ally = reg
        .get_by_name("mod_d_relations::DagAlly")
        .expect("DagAlly registered")
        .handle;
    let rel_sub = reg
        .get_by_name("mod_d_relations::DagSub")
        .expect("DagSub registered")
        .handle;

    let graph = world.resource::<ModRelationGraph>();
    let entity_index = |target: Entity| -> u32 {
        entities
            .iter()
            .position(|&e| e == target)
            .expect("relation endpoint must be a known unit") as u32
    };

    let mut hasher = Fnv64::new();
    hasher.write_i64(trace.acc);
    hasher.write_u32(trace.len);
    hasher.write_u32(trace.observer_hits);
    for &step in &trace_steps {
        hasher.write_u32(step);
    }

    for (idx, &e) in entities.iter().enumerate() {
        let er = world.entity(e);
        let u = er.get::<UnitScore>().expect("UnitScore");
        let a = er.get::<BranchA>().expect("BranchA");
        let b = er.get::<BranchB>().expect("BranchB");
        let c = er.get::<BranchC>().expect("BranchC");
        hasher.write_u32(idx as u32);
        hasher.write_i64(u.score);
        hasher.write_u32(u.id);
        hasher.write_u32(u.flags);
        hasher.write_i64(a.val);
        hasher.write_i64(b.val);
        hasher.write_i64(c.val);
        if let Some(badge) = er.get::<ObservedBadge>() {
            hasher.write_u32(1);
            hasher.write_u32(badge.hits);
            hasher.write_i32(badge.last_delta);
        } else {
            hasher.write_u32(0);
        }
        if let Some(ack) = er.get::<ObserverAck>() {
            hasher.write_u32(1);
            hasher.write_u32(ack.code);
        } else {
            hasher.write_u32(0);
        }
    }

    // Hash canonical sorted relation adjacency & payloads for all 3 relations.
    for (rel_tag, rel_h) in [(1u32, rel_parent), (2u32, rel_ally), (3u32, rel_sub)] {
        hasher.write_u32(rel_tag);
        let mut canonical_edges: Vec<(u32, u32, Vec<u8>)> = Vec::new();
        for (src_idx, &src_e) in entities.iter().enumerate() {
            for edge in graph.outgoing_edges(rel_h, src_e) {
                canonical_edges.push((
                    src_idx as u32,
                    entity_index(edge.target),
                    edge.data.to_vec(),
                ));
            }
        }
        canonical_edges.sort_by(|lhs, rhs| lhs.0.cmp(&rhs.0).then_with(|| lhs.1.cmp(&rhs.1)));
        hasher.write_u32(canonical_edges.len() as u32);
        for (s, t, payload) in canonical_edges {
            hasher.write_u32(s);
            hasher.write_u32(t);
            hasher.write_u32(payload.len() as u32);
            hasher.write_bytes(&payload);
        }
    }

    let cycle_rejections = graph
        .flush_errors()
        .iter()
        .filter(|err| err.status == FfiResult::CYCLE_DETECTED)
        .count();
    hasher.write_u64(cycle_rejections as u64);

    let digest = WorldDigest {
        hash: hasher.finish(),
        trace_steps,
        trace_acc: trace.acc,
        observer_hits: trace.observer_hits,
        parent_edges: graph.edge_count(rel_parent),
        ally_edges: graph.edge_count(rel_ally),
        sub_edges: graph.edge_count(rel_sub),
        cycle_rejections,
    };

    app.unload_all_mods();
    digest
}

#[test]
fn five_mod_dag_bit_exact_world_hash_across_256_shuffle_seeds() {
    let mods = build_five_dag_mods();

    let expected_steps: Vec<u32> = vec![
        // Tick 1
        101, 102, 103, 104, 201, 202, 401, 402, 403, 501, 502, // Tick 2
        101, 102, 103, 104, 201, 202, 401, 402, 403, 501, 502,
    ];

    let mut baseline: Option<WorldDigest> = None;

    for i in 0..NUM_SHUFFLE_SEEDS {
        // Deterministic well-spread 64-bit seeds covering low values, splitmix64 entropy, and edges.
        let seed = (i as u64)
            .wrapping_mul(0x9E37_79B9_7F4A_7C15)
            .wrapping_add(0xD1B5_4A32_D192_ED03);
        let multi_threaded = !i.is_multiple_of(2);

        let digest = run_seed(&mods, seed, multi_threaded);

        assert_eq!(
            digest.trace_steps, expected_steps,
            "seed #{i} ({seed:#x}, multi_threaded={multi_threaded}) violated 5-mod DAG ordering"
        );
        // Observers attached `ObservedBadge` on e0, e3 (from mod_a) and e2 (from mod_d): 3 entities
        // per tick * 2 ticks = 6 total observer badge reads in `e_fold_branches` (3 on tick 1 + 3 on tick 2).
        assert_eq!(
            digest.observer_hits, 6,
            "seed #{i} ({seed:#x}) observer badge hit count mismatch"
        );
        // `DagParent`: e1->e0, e2->e1, e4->e3 (e4->e2 evicted by EXCLUSIVE, e3->e1 removed, e0->e3 rejected) = 3 edges.
        assert_eq!(digest.parent_edges, 3, "seed #{i} DagParent edge count");
        // `DagAlly` (SYMMETRIC): (e0<->e5), (e3<->e7), (e1<->e7) -> 6 directed edges ((e2<->e6) removed).
        assert_eq!(
            digest.ally_edges, 6,
            "seed #{i} DagAlly symmetric edge count"
        );
        // `DagSub`: e6->e5, e7->e6 = 2 edges.
        assert_eq!(digest.sub_edges, 2, "seed #{i} DagSub edge count");
        // Intentional `e0 -> e3` cycle attempt rejected once per tick = 2 `CYCLE_DETECTED` errors.
        assert_eq!(
            digest.cycle_rejections, 2,
            "seed #{i} ACYCLIC cycle rejection count"
        );

        match &baseline {
            None => baseline = Some(digest),
            Some(base) => {
                assert_eq!(
                    digest, *base,
                    "seed #{i} ({seed:#x}, multi_threaded={multi_threaded}) diverged from seed #0 baseline hash {:#018x} vs {:#018x}",
                    digest.hash, base.hash
                );
            }
        }
    }

    let final_base = baseline.expect("256 seeds executed");
    eprintln!(
        "[schedule_shuffle_fuzz] 256/256 seeds bit-exact! hash={:#018x} acc={} steps={} parent_edges={} ally_edges={} sub_edges={} cycle_rejections={}",
        final_base.hash,
        final_base.trace_acc,
        final_base.trace_steps.len(),
        final_base.parent_edges,
        final_base.ally_edges,
        final_base.sub_edges,
        final_base.cycle_rejections,
    );
}

#[test]
fn shuffle_seed_actually_permutes_parallel_set_order_across_seeds() {
    use bevy_ecs::schedule::Schedule;
    use std::collections::BTreeSet;
    use std::sync::{Arc, Mutex};

    let mut observed_orders: BTreeSet<Vec<u32>> = BTreeSet::new();

    for seed in 0..32u64 {
        let log = Arc::new(Mutex::new(Vec::<u32>::new()));
        let mut world = bevy_ecs::world::World::new();
        let mut schedule = Schedule::default();
        schedule.set_executor(SingleThreadedExecutor::new());
        schedule.set_build_settings(ScheduleBuildSettings {
            shuffle_seed: Some(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1)),
            ..Default::default()
        });

        for tag in 0..5u32 {
            let log_c = Arc::clone(&log);
            schedule.add_systems(move || {
                log_c.lock().expect("lock").push(tag);
            });
        }
        schedule.initialize(&mut world).expect("schedule build");
        schedule.run(&mut world);
        observed_orders.insert(log.lock().expect("lock").clone());
    }

    assert!(
        observed_orders.len() >= 8,
        "Bevy 0.20.1 ScheduleBuildSettings::shuffle_seed must produce diverse topological permutations (saw only {})",
        observed_orders.len()
    );
}
