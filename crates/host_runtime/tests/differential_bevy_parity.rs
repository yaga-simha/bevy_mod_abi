#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Differential Native-vs-Dynamic Bevy ECS Parity Harness.
//!
//! Verifies that `DynamicCSystem`, `HostChunkIter` (dense `Table` + archetypal `SparseSet`),
//! `CommandQueueBridge`, and per-`World` symbol/drop registries behave identically to native
//! Bevy 0.20 systems (`Query`, `QueryBuilder`, `Commands`, `ChildOf`, `EntityCloner`):
//!
//! 1. **Access-Compatibility Matrix Parity**: Random single- and multi-query specs mixing
//!    `Table`, `SparseSet`, ZST, and 32-byte over-aligned components across `READ`, `WRITE`,
//!    `FILTER_ONLY`, and `WITHOUT` modes compared against native `QueryBuilder` / `QueryState`
//!    self-conflicts, pairwise `FilteredAccessSet::is_compatible` matrices, and Bevy's schedule
//!    ambiguity detector (`LogLevel::Error`).
//! 2. **Side-by-Side Multi-Frame World & Tick-Wraparound Parity**: Identical initial `World`s
//!    driven by native Bevy systems (`World A`) vs `DynamicCSystem`s (`World B`), asserting
//!    bit-identical entity sets, component values, and `Changed<T>` tick observations across
//!    normal multi-frame schedules AND forced `World::check_change_ticks()` `u32::MAX` wraparound.
//! 3. **Hierarchy (`ChildOf`) Recursive Despawn & `EntityCloner` Drop Safety**: Side-by-side
//!    parent/child/grandchild recursive despawn parity and `clone_and_spawn()` verification with
//!    static POD, dynamic POD, and foreign-droppable dynamic components (`ComponentCloneBehavior::Ignore`).
//! 4. **Concurrent Multi-World `std::thread::scope` Executor & Drop Isolation**: Parallel `World`s
//!    running multi-threaded Bevy schedules simultaneously, verifying worker-thread `host_get_proc_address`
//!    isolation and independent `(WorldId, ComponentUuid)` foreign drop-trampoline counts.

use abi_contract::{
    ComponentAccessMode, ComponentUuid, FfiEntity, FfiResult, ModComponentDescriptor, RawChunkIter,
    RawCommandQueue,
};
use bevy_app::{App, Update};
use bevy_ecs::component::{Component, ComponentCloneBehavior};
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::query::{Changed, FilteredAccessSet, QueryBuilder, With, Without};
use bevy_ecs::schedule::{IntoScheduleConfigs, LogLevel, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{Commands, Query, System, SystemAccess};
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use host_runtime::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_readonly, host_iter_entities, host_iter_next_chunk,
    host_iter_reset, host_register_component, reset_drop_trampolines_for_testing, AbiModPlugin,
    CommandQueueBridge, ComponentAccessDescOwned, DynamicCSystem, ModAppExt, ModInitContext,
    ModRegistry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

// ============================================================================
// Pure-Rust Deterministic PRNG
// ============================================================================

#[derive(Clone)]
struct Rng(u64);

impl Rng {
    const fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn range(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive <= 1 {
            0
        } else {
            (self.next_u64() as usize) % upper_exclusive
        }
    }

    fn gen_bool(&mut self) -> bool {
        (self.next_u64() & 1) != 0
    }
}

// ============================================================================
// Heterogeneous Component Schema (Table, SparseSet, ZST, Over-Aligned)
// ============================================================================

#[repr(C)]
#[derive(
    Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct DenseVec {
    pub x: i64,
    pub y: i64,
}

#[repr(C, align(32))]
#[derive(
    Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,
)]
pub struct OverAligned32 {
    pub lanes: [i64; 4],
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct DenseZst;

#[repr(C, align(16))]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq)]
pub struct Aligned16Zst;

#[repr(C)]
#[derive(
    Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,
)]
#[component(storage = "SparseSet")]
pub struct SparseEnergy {
    pub energy: i64,
}

#[repr(C)]
#[derive(
    Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord,
)]
#[component(storage = "SparseSet")]
pub struct SparseBadge {
    pub tier: u32,
}

#[repr(C)]
#[derive(Component, host_runtime::ModExposable, Debug, Clone, Copy, PartialEq, Eq)]
#[component(storage = "SparseSet")]
pub struct SparseZst;

const SCHEMA_NAMES: [&str; 7] = [
    "DenseVec",
    "OverAligned32",
    "DenseZst",
    "Aligned16Zst",
    "SparseEnergy",
    "SparseBadge",
    "SparseZst",
];

fn register_full_schema(app: &mut App) {
    app.register_mod_component::<DenseVec>();
    app.register_mod_component::<OverAligned32>();
    app.register_mod_component::<DenseZst>();
    app.register_mod_component::<Aligned16Zst>();
    app.register_mod_component::<SparseEnergy>();
    app.register_mod_component::<SparseBadge>();
    app.register_mod_component::<SparseZst>();
}

unsafe extern "C" fn noop_single_cb(
    _iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn noop_multi_cb(
    _iters: *const *mut RawChunkIter,
    _count: usize,
    _commands: *mut RawCommandQueue,
    _user_data: *mut c_void,
) -> FfiResult {
    FfiResult::OK
}

// ============================================================================
// Suite 1: Random Query & Access-Compatibility Matrix Parity
// ============================================================================

/// Computes native Bevy `FilteredAccessSet` and self-conflict flag for a multi-query specification,
/// matching Bevy's intra-query (`&T` + `&mut T` or duplicate `&mut T`) and inter-query borrow rules.
fn build_native_reference_access(
    world: &mut World,
    registry: &ModRegistry,
    groups: &[Vec<ComponentAccessDescOwned>],
) -> (bool, FilteredAccessSet) {
    let mut combined = FilteredAccessSet::new();

    for group in groups {
        let mut seen_read = BTreeSet::new();
        let mut seen_write = BTreeSet::new();

        let mut builder = QueryBuilder::<()>::new(world);
        for clause in group {
            let comp_id = registry
                .get_by_name(&clause.name)
                .expect("registered schema component")
                .component_id;
            match clause.mode {
                ComponentAccessMode::READ => {
                    if seen_write.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    seen_read.insert(comp_id);
                    builder.ref_id(comp_id);
                }
                ComponentAccessMode::WRITE => {
                    if seen_read.contains(&comp_id) || seen_write.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    seen_write.insert(comp_id);
                    builder.mut_id(comp_id);
                }
                ComponentAccessMode::FILTER_ONLY => {
                    builder.with_id(comp_id);
                }
                ComponentAccessMode::WITHOUT => {
                    builder.without_id(comp_id);
                }
                _ => unreachable!("valid mode generated"),
            }
        }

        let state = builder.build();
        // QueryBuilder may also detect contradictory intra-query filters (e.g. With<A> + Without<A>
        // makes a query match nothing, which is NOT a conflict, whereas &mut A + &A was caught above).
        let fa = state.component_access().clone();
        let mut candidate = FilteredAccessSet::new();
        candidate.add(fa.clone());
        if !combined.is_compatible(&candidate) {
            return (true, FilteredAccessSet::new());
        }
        combined.add(fa);
    }

    (false, combined)
}

#[test]
fn differential_query_access_compatibility_matrix_parity() {
    #[cfg(not(miri))]
    const NUM_SPECS: usize = 80;
    #[cfg(miri)]
    const NUM_SPECS: usize = 20;

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    register_full_schema(&mut app);

    // Seed diverse archetypes (including shared dense tables differentiated only by SparseSet components)
    app.world_mut().spawn((
        DenseVec { x: 1, y: 2 },
        OverAligned32 {
            lanes: [1, 2, 3, 4],
        },
    ));
    app.world_mut().spawn((
        DenseVec { x: 3, y: 4 },
        OverAligned32 {
            lanes: [5, 6, 7, 8],
        },
        SparseEnergy { energy: 10 },
        SparseZst,
    ));
    app.world_mut().spawn((
        DenseVec { x: 5, y: 6 },
        DenseZst,
        Aligned16Zst,
        SparseBadge { tier: 2 },
    ));

    let reg = Arc::new(app.world().resource::<ModRegistry>().clone());
    let modes = [
        ComponentAccessMode::READ,
        ComponentAccessMode::WRITE,
        ComponentAccessMode::FILTER_ONLY,
        ComponentAccessMode::WITHOUT,
    ];

    let mut rng = Rng::new(0xA11C_E550_2026_0001);
    let mut valid_dyn_sets: Vec<(Vec<Vec<ComponentAccessDescOwned>>, FilteredAccessSet)> =
        Vec::new();
    let mut valid_native_sets: Vec<FilteredAccessSet> = Vec::new();
    let mut observed_self_conflicts = 0usize;

    for idx in 0..NUM_SPECS {
        let group_count = if idx % 3 == 0 {
            1
        } else {
            1 + rng.range(4) // 1..=4 query groups
        };
        let mut groups = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let clause_count = 1 + rng.range(5);
            let mut clauses = Vec::with_capacity(clause_count);
            for _ in 0..clause_count {
                clauses.push(ComponentAccessDescOwned {
                    name: SCHEMA_NAMES[rng.range(SCHEMA_NAMES.len())].to_string(),
                    mode: modes[rng.range(modes.len())],
                });
            }
            groups.push(clauses);
        }

        let mut dyn_sys = if groups.len() == 1 {
            DynamicCSystem::new(
                format!("gen_single_{idx}"),
                groups[0].clone(),
                noop_single_cb,
                core::ptr::null_mut(),
                Arc::clone(&reg),
            )
        } else {
            DynamicCSystem::new_multi_query(
                format!("gen_multi_{idx}"),
                groups.clone(),
                noop_multi_cb,
                core::ptr::null_mut(),
                Arc::clone(&reg),
            )
        };

        let dyn_sys_access = System::initialize(&mut dyn_sys, app.world_mut());
        let (native_conflict, native_set) =
            build_native_reference_access(app.world_mut(), &reg, &groups);

        assert_eq!(
            dyn_sys.has_self_conflict(),
            native_conflict,
            "Self-conflict mismatch on generated spec #{idx}: {groups:?}"
        );
        assert_eq!(dyn_sys.is_enabled(), !native_conflict);

        if native_conflict {
            observed_self_conflicts += 1;
            assert!(
                matches!(dyn_sys_access, SystemAccess::None),
                "Self-conflicting DynamicCSystem must report SystemAccess::None"
            );
        } else {
            let SystemAccess::Shared(dyn_set) = dyn_sys_access else {
                panic!("Valid DynamicCSystem must return SystemAccess::Shared");
            };
            valid_dyn_sets.push((groups, dyn_set));
            valid_native_sets.push(native_set);
        }
    }

    assert!(
        observed_self_conflicts > 0 && valid_dyn_sets.len() >= 10,
        "Generator must exercise both self-conflicting ({observed_self_conflicts}) and valid ({}) systems",
        valid_dyn_sets.len()
    );

    // Pairwise compatibility matrix parity across all valid generated systems!
    let mut compatible_pairs = 0usize;
    let mut conflicting_pairs = 0usize;
    for i in 0..valid_dyn_sets.len() {
        for j in 0..valid_dyn_sets.len() {
            let dyn_compat = valid_dyn_sets[i].1.is_compatible(&valid_dyn_sets[j].1);
            let native_compat = valid_native_sets[i].is_compatible(&valid_native_sets[j]);
            assert_eq!(
                dyn_compat, native_compat,
                "Compatibility mismatch for pair ({i}, {j}):\n  lhs={:?}\n  rhs={:?}",
                valid_dyn_sets[i].0, valid_dyn_sets[j].0
            );
            if dyn_compat {
                compatible_pairs += 1;
            } else {
                conflicting_pairs += 1;
            }
        }
    }
    assert!(compatible_pairs > 0 && conflicting_pairs > 0);

    // Verify actual Bevy Schedule ambiguity detector (`LogLevel::Error`) accepts disjoint
    // Table + SparseSet + ZST systems identically for DynamicCSystem and native systems!
    let mut strict_schedule = Schedule::default();
    strict_schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });

    let dyn_disjoint_a = DynamicCSystem::new(
        "disjoint_a".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "DenseVec".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "SparseBadge".to_string(),
                mode: ComponentAccessMode::FILTER_ONLY,
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg),
    );
    let dyn_disjoint_b = DynamicCSystem::new(
        "disjoint_b".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "DenseVec".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "SparseBadge".to_string(),
                mode: ComponentAccessMode::WITHOUT,
            },
        ],
        noop_single_cb,
        core::ptr::null_mut(),
        Arc::clone(&reg),
    );
    strict_schedule.add_systems((dyn_disjoint_a, dyn_disjoint_b));
    strict_schedule
        .initialize(app.world_mut())
        .expect("Disjoint SparseSet With/Without DynamicCSystems must have zero ambiguity");
}

// ============================================================================
// Suite 2: Side-by-Side World Execution & Tick-Wraparound Parity
// ============================================================================

#[derive(Debug, Clone, PartialEq, Eq)]
struct EntitySnapshot {
    dense_vec: Option<DenseVec>,
    over_aligned: Option<OverAligned32>,
    has_dense_zst: bool,
    has_aligned16_zst: bool,
    sparse_energy: Option<SparseEnergy>,
    sparse_badge: Option<SparseBadge>,
    has_sparse_zst: bool,
}

fn snapshot_world(world: &mut World) -> BTreeMap<u64, EntitySnapshot> {
    let mut map = BTreeMap::new();
    let mut q = world.query::<Entity>();
    let entities: Vec<Entity> = q.iter(world).collect();
    for e in entities {
        let er = world.entity(e);
        // Ignore internal Bevy observer/schedule entities that hold none of our schema components
        let snap = EntitySnapshot {
            dense_vec: er.get::<DenseVec>().copied(),
            over_aligned: er.get::<OverAligned32>().copied(),
            has_dense_zst: er.contains::<DenseZst>(),
            has_aligned16_zst: er.contains::<Aligned16Zst>(),
            sparse_energy: er.get::<SparseEnergy>().copied(),
            sparse_badge: er.get::<SparseBadge>().copied(),
            has_sparse_zst: er.contains::<SparseZst>(),
        };
        if snap.dense_vec.is_some()
            || snap.over_aligned.is_some()
            || snap.has_dense_zst
            || snap.has_aligned16_zst
            || snap.sparse_energy.is_some()
            || snap.sparse_badge.is_some()
            || snap.has_sparse_zst
        {
            map.insert(e.to_bits(), snap);
        }
    }
    map
}

#[derive(Default, Debug, Clone, PartialEq, Eq)]
struct ChangedReport {
    dense_vec_bits: BTreeSet<u64>,
    over_aligned_bits: BTreeSet<u64>,
    sparse_energy_bits: BTreeSet<u64>,
}

struct PipelineControl {
    mutate_enabled: AtomicBool,
    frame_number: AtomicUsize,
}

// --- Dynamic callbacks mirroring the 4-stage native pipeline ---

/// Stage 1: `WRITE DenseVec`, `READ OverAligned32`, `WITHOUT SparseZst`
unsafe extern "C" fn dyn_stage1_dense_without_sparse_zst(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `PipelineControl` valid for the test's lifetime.
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    loop {
        // SAFETY: Valid `iter`.
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        // SAFETY: Col 0 = WRITE DenseVec, Col 1 = READ OverAligned32.
        let dv_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut DenseVec;
        let oa_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const OverAligned32;
        assert!(!dv_ptr.is_null() && !oa_ptr.is_null());
        assert_eq!(
            (oa_ptr as usize) % core::mem::align_of::<OverAligned32>(),
            0,
            "32-byte over-aligned column pointer must preserve exact 32-byte alignment"
        );
        for row in 0..count {
            // SAFETY: `row < count` within chunk bounds.
            unsafe {
                let oa = &*oa_ptr.add(row);
                let dv = &mut *dv_ptr.add(row);
                dv.x = dv.x.wrapping_add(oa.lanes[0]);
                dv.y = dv.y.wrapping_sub(oa.lanes[1]);
            }
        }
    }
    FfiResult::OK
}

/// Stage 2: `WRITE SparseEnergy`, `READ DenseVec`, `FILTER_ONLY SparseBadge`
unsafe extern "C" fn dyn_stage2_sparse_data_rw(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `PipelineControl`.
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    loop {
        // SAFETY: Valid `iter`.
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        // SAFETY: Col 0 = WRITE SparseEnergy, Col 1 = READ DenseVec.
        let se_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut SparseEnergy;
        let dv_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const DenseVec;
        assert!(!se_ptr.is_null() && !dv_ptr.is_null());
        for row in 0..count {
            // SAFETY: `row < count`.
            unsafe {
                (*se_ptr.add(row)).energy = (*se_ptr.add(row))
                    .energy
                    .wrapping_add((*dv_ptr.add(row)).x & 0xFF);
            }
        }
    }
    FfiResult::OK
}

/// Stage 3 (Multi-Query):
/// - Query 0: `READ OverAligned32`, `FILTER_ONLY DenseZst`
/// - Query 1: `WRITE OverAligned32`, `WITHOUT DenseZst` (rewound via `host_iter_reset`!)
unsafe extern "C" fn dyn_stage3_multi_query_nested(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    assert_eq!(iter_count, 2);
    // SAFETY: `user_data` points to `PipelineControl`.
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }

    // SAFETY: `iters` has length 2.
    let q_src = unsafe { *iters.add(0) };
    let q_dst = unsafe { *iters.add(1) };

    let mut sum_lane2: i64 = 0;
    loop {
        // SAFETY: Valid `q_src`.
        let c = unsafe { host_iter_next_chunk(q_src) };
        if c == 0 {
            break;
        }
        // SAFETY: Col 0 = READ OverAligned32.
        let ptr = unsafe { host_iter_column_readonly(q_src, 0) } as *const OverAligned32;
        for r in 0..c {
            // SAFETY: `r < c`.
            sum_lane2 = sum_lane2.wrapping_add(unsafe { (*ptr.add(r)).lanes[2] });
        }
    }

    // Exercise `host_iter_reset` to verify zero-allocation rewind parity
    // SAFETY: Valid `q_dst`.
    assert_eq!(unsafe { host_iter_reset(q_dst) }, FfiResult::OK);
    loop {
        // SAFETY: Valid `q_dst`.
        let c = unsafe { host_iter_next_chunk(q_dst) };
        if c == 0 {
            break;
        }
        // SAFETY: Col 0 = WRITE OverAligned32.
        let ptr = unsafe { host_iter_column_mutable(q_dst, 0) } as *mut OverAligned32;
        for r in 0..c {
            // SAFETY: `r < c`.
            unsafe {
                (*ptr.add(r)).lanes[3] = (*ptr.add(r)).lanes[3].wrapping_add(sum_lane2);
            }
        }
    }
    FfiResult::OK
}

/// Stage 4 (`Commands` structural parity):
/// Despawns entities with `SparseEnergy.energy > 250` (`WITHOUT DenseZst`), and spawns 1 fresh
/// entity carrying `DenseVec`, `OverAligned32`, and `Aligned16Zst` when `mutate_enabled` is true.
unsafe extern "C" fn dyn_stage4_commands(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `PipelineControl`.
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    let frame = ctrl.frame_number.load(Ordering::SeqCst) as i64;

    loop {
        // SAFETY: Valid `iter`.
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        // SAFETY: Col 0 = READ SparseEnergy, entities slice valid for `count`.
        let se_ptr = unsafe { host_iter_column_readonly(iter, 0) } as *const SparseEnergy;
        let ents = unsafe { host_iter_entities(iter) };
        for r in 0..count {
            // SAFETY: `r < count`.
            let energy = unsafe { (*se_ptr.add(r)).energy };
            if energy > 250 {
                let target = unsafe { *ents.add(r) };
                assert_eq!(unsafe { host_cmd_despawn(commands, target) }, FfiResult::OK);
            }
        }
    }

    // Spawn 1 new entity via C-ABI commands matching native `commands.spawn(...)`
    // SAFETY: Valid `commands` and `#[repr(C)]` POD/ZST components.
    unsafe {
        let e = host_cmd_spawn_empty(commands);
        assert!(!e.is_null());
        let dv = DenseVec {
            x: 10 + frame,
            y: 20 - frame,
        };
        let oa = OverAligned32 {
            lanes: [1, 1, frame, 0],
        };
        assert_eq!(
            host_cmd_insert_component(
                commands,
                e,
                c"DenseVec".as_ptr(),
                (&dv as *const DenseVec).cast::<u8>(),
                core::mem::size_of::<DenseVec>(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_insert_component(
                commands,
                e,
                c"OverAligned32".as_ptr(),
                (&oa as *const OverAligned32).cast::<u8>(),
                core::mem::size_of::<OverAligned32>(),
            ),
            FfiResult::OK
        );
        assert_eq!(
            host_cmd_insert_component(commands, e, c"Aligned16Zst".as_ptr(), core::ptr::null(), 0),
            FfiResult::OK
        );
    }

    FfiResult::OK
}

fn seed_world_deterministically(world: &mut World, seed: u64, count: usize) {
    let mut rng = Rng::new(seed);
    for i in 0..count {
        let dv = DenseVec {
            x: (rng.next_u64() % 50) as i64,
            y: (rng.next_u64() % 50) as i64,
        };
        let oa = OverAligned32 {
            lanes: [
                1 + (rng.range(5) as i64),
                (rng.range(4) as i64),
                1 + (rng.range(3) as i64),
                (i as i64),
            ],
        };

        let mut emut = world.spawn(dv);
        if rng.range(5) != 0 {
            emut.insert(oa);
        }
        if rng.gen_bool() {
            emut.insert(DenseZst);
        }
        if rng.gen_bool() {
            emut.insert(Aligned16Zst);
        }
        if rng.gen_bool() {
            emut.insert(SparseEnergy {
                energy: 180 + (rng.range(90) as i64),
            });
        }
        if rng.gen_bool() {
            emut.insert(SparseBadge {
                tier: rng.range(4) as u32,
            });
        }
        if rng.range(3) == 0 {
            emut.insert(SparseZst);
        }
    }
}

#[test]
fn differential_side_by_side_multiframe_execution_and_tick_wraparound_parity() {
    #[cfg(not(miri))]
    const INITIAL_ENTITIES: usize = 64;
    #[cfg(miri)]
    const INITIAL_ENTITIES: usize = 16;

    let mut app_native = App::new();
    app_native.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_native);

    let mut app_dynamic = App::new();
    app_dynamic.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_dynamic);

    const SEED: u64 = 0x51DE_B151_DE20_2602;
    seed_world_deterministically(app_native.world_mut(), SEED, INITIAL_ENTITIES);
    seed_world_deterministically(app_dynamic.world_mut(), SEED, INITIAL_ENTITIES);

    // Confirm initial snapshots match 100% before frame 1
    assert_eq!(
        snapshot_world(app_native.world_mut()),
        snapshot_world(app_dynamic.world_mut())
    );

    let ctrl = Arc::new(PipelineControl {
        mutate_enabled: AtomicBool::new(true),
        frame_number: AtomicUsize::new(0),
    });

    let report_native = Arc::new(Mutex::new(ChangedReport::default()));
    let report_dynamic = Arc::new(Mutex::new(ChangedReport::default()));

    // Wire native World A pipeline
    let ctrl_n1 = Arc::clone(&ctrl);
    let native_s1 = move |mut q: Query<(&mut DenseVec, &OverAligned32), Without<SparseZst>>| {
        if !ctrl_n1.mutate_enabled.load(Ordering::SeqCst) {
            return;
        }
        for (mut dv, oa) in &mut q {
            dv.x = dv.x.wrapping_add(oa.lanes[0]);
            dv.y = dv.y.wrapping_sub(oa.lanes[1]);
        }
    };

    let ctrl_n2 = Arc::clone(&ctrl);
    let native_s2 = move |mut q: Query<(&mut SparseEnergy, &DenseVec), With<SparseBadge>>| {
        if !ctrl_n2.mutate_enabled.load(Ordering::SeqCst) {
            return;
        }
        for (mut se, dv) in &mut q {
            se.energy = se.energy.wrapping_add(dv.x & 0xFF);
        }
    };

    let ctrl_n3 = Arc::clone(&ctrl);
    let native_s3 =
        move |q_src: Query<&OverAligned32, With<DenseZst>>,
              mut q_dst: Query<&mut OverAligned32, Without<DenseZst>>| {
            if !ctrl_n3.mutate_enabled.load(Ordering::SeqCst) {
                return;
            }
            let mut sum_lane2: i64 = 0;
            for oa in &q_src {
                sum_lane2 = sum_lane2.wrapping_add(oa.lanes[2]);
            }
            for mut oa in &mut q_dst {
                oa.lanes[3] = oa.lanes[3].wrapping_add(sum_lane2);
            }
        };

    let ctrl_n4 = Arc::clone(&ctrl);
    let native_s4 = move |q: Query<(Entity, &SparseEnergy), Without<DenseZst>>,
                          mut commands: Commands| {
        if !ctrl_n4.mutate_enabled.load(Ordering::SeqCst) {
            return;
        }
        let frame = ctrl_n4.frame_number.load(Ordering::SeqCst) as i64;
        for (e, se) in &q {
            if se.energy > 250 {
                commands.entity(e).despawn();
            }
        }
        let mut spawned = commands.spawn_empty();
        spawned.insert(DenseVec {
            x: 10 + frame,
            y: 20 - frame,
        });
        spawned.insert(OverAligned32 {
            lanes: [1, 1, frame, 0],
        });
        spawned.insert(Aligned16Zst);
    };

    let rep_n = Arc::clone(&report_native);
    let native_observer =
        move |q_dv: Query<Entity, Changed<DenseVec>>,
              q_oa: Query<Entity, Changed<OverAligned32>>,
              q_se: Query<Entity, Changed<SparseEnergy>>| {
            let mut r = rep_n.lock().unwrap();
            r.dense_vec_bits = q_dv.iter().map(Entity::to_bits).collect();
            r.over_aligned_bits = q_oa.iter().map(Entity::to_bits).collect();
            r.sparse_energy_bits = q_se.iter().map(Entity::to_bits).collect();
        };

    app_native.add_systems(
        Update,
        (native_s1, native_s2, native_s3, native_s4, native_observer).chain(),
    );

    // Wire dynamic World B pipeline (`DynamicCSystem` single + multi-query + commands)
    let reg_b = Arc::new(app_dynamic.world().resource::<ModRegistry>().clone());
    let ud = Arc::as_ptr(&ctrl) as *mut c_void;

    let dyn_s1 = DynamicCSystem::new(
        "dyn_s1".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "DenseVec".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "OverAligned32".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "SparseZst".to_string(),
                mode: ComponentAccessMode::WITHOUT,
            },
        ],
        dyn_stage1_dense_without_sparse_zst,
        ud,
        Arc::clone(&reg_b),
    );

    let dyn_s2 = DynamicCSystem::new(
        "dyn_s2".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "SparseEnergy".to_string(),
                mode: ComponentAccessMode::WRITE,
            },
            ComponentAccessDescOwned {
                name: "DenseVec".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "SparseBadge".to_string(),
                mode: ComponentAccessMode::FILTER_ONLY,
            },
        ],
        dyn_stage2_sparse_data_rw,
        ud,
        Arc::clone(&reg_b),
    );

    let dyn_s3 = DynamicCSystem::new_multi_query(
        "dyn_s3".to_string(),
        vec![
            vec![
                ComponentAccessDescOwned {
                    name: "OverAligned32".to_string(),
                    mode: ComponentAccessMode::READ,
                },
                ComponentAccessDescOwned {
                    name: "DenseZst".to_string(),
                    mode: ComponentAccessMode::FILTER_ONLY,
                },
            ],
            vec![
                ComponentAccessDescOwned {
                    name: "OverAligned32".to_string(),
                    mode: ComponentAccessMode::WRITE,
                },
                ComponentAccessDescOwned {
                    name: "DenseZst".to_string(),
                    mode: ComponentAccessMode::WITHOUT,
                },
            ],
        ],
        dyn_stage3_multi_query_nested,
        ud,
        Arc::clone(&reg_b),
    );

    let dyn_s4 = DynamicCSystem::new(
        "dyn_s4".to_string(),
        vec![
            ComponentAccessDescOwned {
                name: "SparseEnergy".to_string(),
                mode: ComponentAccessMode::READ,
            },
            ComponentAccessDescOwned {
                name: "DenseZst".to_string(),
                mode: ComponentAccessMode::WITHOUT,
            },
        ],
        dyn_stage4_commands,
        ud,
        Arc::clone(&reg_b),
    );

    let rep_d = Arc::clone(&report_dynamic);
    let dyn_observer = move |q_dv: Query<Entity, Changed<DenseVec>>,
                             q_oa: Query<Entity, Changed<OverAligned32>>,
                             q_se: Query<Entity, Changed<SparseEnergy>>| {
        let mut r = rep_d.lock().unwrap();
        r.dense_vec_bits = q_dv.iter().map(Entity::to_bits).collect();
        r.over_aligned_bits = q_oa.iter().map(Entity::to_bits).collect();
        r.sparse_energy_bits = q_se.iter().map(Entity::to_bits).collect();
    };

    app_dynamic.add_systems(
        Update,
        (dyn_s1, dyn_s2, dyn_s3, dyn_s4, dyn_observer).chain(),
    );

    // Phase A: Normal multi-frame schedule execution alternating active & idle frames
    for frame in 1..=8usize {
        ctrl.frame_number.store(frame, Ordering::SeqCst);
        // Gate off mutations on frames 4 and 7 to verify Changed<T> drops cleanly to empty!
        let active = frame != 4 && frame != 7;
        ctrl.mutate_enabled.store(active, Ordering::SeqCst);

        app_native.update();
        app_dynamic.update();

        let ch_native = report_native.lock().unwrap().clone();
        let ch_dynamic = report_dynamic.lock().unwrap().clone();
        assert_eq!(
            ch_native, ch_dynamic,
            "Changed<T> entity set parity mismatch on normal frame {frame} (active={active})"
        );
        if !active {
            assert!(
                ch_dynamic.dense_vec_bits.is_empty()
                    && ch_dynamic.over_aligned_bits.is_empty()
                    && ch_dynamic.sparse_energy_bits.is_empty(),
                "Idle frame {frame} must report zero Changed<T> entities"
            );
        }

        assert_eq!(
            snapshot_world(app_native.world_mut()),
            snapshot_world(app_dynamic.world_mut()),
            "World snapshot parity mismatch after frame {frame}"
        );
    }

    // Phase B1: Real `World::check_change_ticks()` clamping pass across Tables, SparseSets, & Schedules!
    // Increment both worlds past `CHECK_TICK_THRESHOLD` so `world.check_change_ticks()` returns `Some(CheckChangeTicks)`
    // and clamps every component column tick AND invokes `DynamicCSystem::check_change_tick`!
    #[cfg(not(miri))]
    {
        use bevy_ecs::change_detection::CHECK_TICK_THRESHOLD;
        for app in [&mut app_native, &mut app_dynamic] {
            let world = app.world_mut();
            for _ in 0..CHECK_TICK_THRESHOLD {
                world.increment_change_tick();
            }
            let check = world.check_change_ticks();
            assert!(
                check.is_some(),
                "Advancing CHECK_TICK_THRESHOLD ticks must trigger a real World::check_change_ticks() scan"
            );
        }

        for post_scan_step in 1..=3usize {
            ctrl.frame_number
                .store(100 + post_scan_step, Ordering::SeqCst);
            // Step 1 = idle (must observe 0 spurious Changed<T> after tick clamping!), Step 2 = active, Step 3 = idle
            let active = post_scan_step == 2;
            ctrl.mutate_enabled.store(active, Ordering::SeqCst);

            app_native.update();
            app_dynamic.update();

            let ch_native = report_native.lock().unwrap().clone();
            let ch_dynamic = report_dynamic.lock().unwrap().clone();
            assert_eq!(
                ch_native, ch_dynamic,
                "Changed<T> parity mismatch after World::check_change_ticks() scan step {post_scan_step} (active={active})"
            );
            if !active {
                assert!(
                    ch_dynamic.dense_vec_bits.is_empty()
                        && ch_dynamic.over_aligned_bits.is_empty()
                        && ch_dynamic.sparse_energy_bits.is_empty(),
                    "Post-check_change_ticks idle frame must report zero false-positive Changed<T> entities"
                );
            }
            assert_eq!(
                snapshot_world(app_native.world_mut()),
                snapshot_world(app_dynamic.world_mut()),
                "World snapshot parity mismatch after World::check_change_ticks() step {post_scan_step}"
            );
        }
    }

    // Phase B2: Direct `u32::MAX` -> `0` wraparound boundary parity on `System::set_last_run` & component ticks
    // (runs in microseconds even under Miri!).
    {
        use bevy_ecs::change_detection::{DetectChangesMut, Tick, MAX_CHANGE_AGE};
        use bevy_ecs::system::IntoSystem;

        for boundary_last_run in [
            u32::MAX - 5,
            u32::MAX - 1,
            u32::MAX,
            0u32,
            1u32,
            10u32.wrapping_sub(MAX_CHANGE_AGE),
        ] {
            let mut w_nat = World::new();
            w_nat.insert_resource(ModRegistry::new());
            let mut w_dyn = World::new();
            w_dyn.insert_resource(ModRegistry::new());

            let dv_id_d = w_dyn.register_component::<DenseVec>();
            let oa_id_d = w_dyn.register_component::<OverAligned32>();
            {
                let mut reg = w_dyn.resource_mut::<ModRegistry>();
                reg.register_component::<DenseVec>(dv_id_d);
                reg.register_component::<OverAligned32>(oa_id_d);
            }
            w_nat.register_component::<DenseVec>();
            w_nat.register_component::<OverAligned32>();

            let e_nat = w_nat.spawn(DenseVec { x: 7, y: 8 }).id();
            let e_dyn = w_dyn.spawn(DenseVec { x: 7, y: 8 }).id();

            // Stamp boundary component ticks near `u32::MAX` wraparound on both worlds
            let stamped_tick = Tick::new(boundary_last_run.wrapping_add(1));
            w_nat
                .entity_mut(e_nat)
                .get_mut::<DenseVec>()
                .unwrap()
                .set_last_changed(stamped_tick);
            w_dyn
                .entity_mut(e_dyn)
                .get_mut::<DenseVec>()
                .unwrap()
                .set_last_changed(stamped_tick);

            let mut nat_sys = IntoSystem::into_system(|mut q: Query<&mut DenseVec>| {
                for mut v in &mut q {
                    v.x += 3;
                }
            });
            let reg_d = Arc::new(w_dyn.resource::<ModRegistry>().clone());
            let ctrl_once = PipelineControl {
                mutate_enabled: AtomicBool::new(true),
                frame_number: AtomicUsize::new(1),
            };
            // Spawn OverAligned32 too so dyn_stage1 matches
            w_nat.entity_mut(e_nat).insert(OverAligned32 {
                lanes: [3, 0, 0, 0],
            });
            w_dyn.entity_mut(e_dyn).insert(OverAligned32 {
                lanes: [3, 0, 0, 0],
            });

            let mut dyn_sys = DynamicCSystem::new(
                "wrap_boundary_dyn".to_string(),
                vec![
                    ComponentAccessDescOwned {
                        name: "DenseVec".to_string(),
                        mode: ComponentAccessMode::WRITE,
                    },
                    ComponentAccessDescOwned {
                        name: "OverAligned32".to_string(),
                        mode: ComponentAccessMode::READ,
                    },
                ],
                dyn_stage1_dense_without_sparse_zst,
                (&ctrl_once as *const PipelineControl) as *mut c_void,
                reg_d,
            );

            nat_sys.initialize(&mut w_nat);
            dyn_sys.initialize(&mut w_dyn);
            nat_sys.set_last_run(Tick::new(boundary_last_run));
            dyn_sys.set_last_run(Tick::new(boundary_last_run));

            let mut obs_nat =
                IntoSystem::into_system(|q: Query<Entity, Changed<DenseVec>>| q.iter().count());
            let mut obs_dyn =
                IntoSystem::into_system(|q: Query<Entity, Changed<DenseVec>>| q.iter().count());
            obs_nat.initialize(&mut w_nat);
            obs_dyn.initialize(&mut w_dyn);
            obs_nat.set_last_run(Tick::new(boundary_last_run));
            obs_dyn.set_last_run(Tick::new(boundary_last_run));

            nat_sys.run((), &mut w_nat).unwrap();
            dyn_sys.run((), &mut w_dyn).unwrap();

            let c_nat = obs_nat.run((), &mut w_nat).unwrap();
            let c_dyn = obs_dyn.run((), &mut w_dyn).unwrap();
            assert_eq!(
                c_nat, c_dyn,
                "Wraparound boundary last_run={boundary_last_run:#x} mismatch"
            );
            assert_eq!(w_nat.get::<DenseVec>(e_nat), w_dyn.get::<DenseVec>(e_dyn));
        }
    }
}

// ============================================================================
// Suite 3: Hierarchy (`ChildOf`) Recursive Despawn & `EntityCloner` Drop Safety
// ============================================================================

static HIERARCHY_DYN_DROPS: AtomicUsize = AtomicUsize::new(0);
static CLONER_DYN_DROPS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn count_hierarchy_drop(_ptr: *mut c_void) {
    HIERARCHY_DYN_DROPS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn count_cloner_drop(_ptr: *mut c_void) {
    CLONER_DYN_DROPS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn despawn_target_entity_cb(
    _iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to an `FfiEntity` containing the root entity to despawn.
    let target = unsafe { *(user_data as *const FfiEntity) };
    // SAFETY: Valid `commands` handle.
    unsafe { host_cmd_despawn(commands, target) }
}

#[test]
fn differential_hierarchy_child_of_despawn_and_entity_cloner_drop_parity() {
    reset_drop_trampolines_for_testing();
    HIERARCHY_DYN_DROPS.store(0, Ordering::SeqCst);
    CLONER_DYN_DROPS.store(0, Ordering::SeqCst);

    // --- Part A: `ChildOf` Hierarchy Recursive Despawn Parity (World A Native vs World B Dynamic) ---
    let mut app_native = App::new();
    app_native.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_native);

    let mut app_dynamic = App::new();
    app_dynamic.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_dynamic);

    // Register POD (`DynPodStat`) and droppable (`DynDropRes`) runtime components on World B
    let (dyn_pod_id, dyn_drop_id) = {
        let world = app_dynamic.world_mut();
        let reg = Arc::new(world.resource::<ModRegistry>().clone());
        let active = Arc::new(AtomicBool::new(true));
        let mut ctx = ModInitContext::with_world(reg, active, world as *mut World);
        let _g = ctx.make_active();
        let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();

        let pod_desc = ModComponentDescriptor::new(
            c"DynPodStat".as_ptr(),
            ComponentUuid::from_name("DynPodStat"),
            16,
            8,
        );
        let drop_desc = ModComponentDescriptor::new(
            c"DynDropRes".as_ptr(),
            ComponentUuid::from_name("DynDropRes"),
            8,
            8,
        )
        .with_drop(count_hierarchy_drop);

        // SAFETY: Valid `ctx_ptr` and descriptors.
        unsafe {
            assert_eq!(host_register_component(ctx_ptr, &pod_desc), FfiResult::OK);
            assert_eq!(host_register_component(ctx_ptr, &drop_desc), FfiResult::OK);
        }
        drop(_g);

        let r = world.resource::<ModRegistry>();
        (
            r.get_by_name("DynPodStat").unwrap().component_id,
            r.get_by_name("DynDropRes").unwrap().component_id,
        )
    };

    // Helper building identical 5-node hierarchy (`root -> [c1 -> [gc1, gc2], c2]`) + 2-node bystander tree
    let spawn_trees =
        |world: &mut World, attach_dynamic: bool| -> (Entity, Vec<Entity>, Vec<Entity>) {
            let root = world.spawn(DenseVec { x: 100, y: 0 }).id();
            let c1 = world.spawn((DenseVec { x: 110, y: 1 }, ChildOf(root))).id();
            let gc1 = world.spawn((DenseVec { x: 111, y: 2 }, ChildOf(c1))).id();
            let gc2 = world.spawn((DenseVec { x: 112, y: 2 }, ChildOf(c1))).id();
            let c2 = world.spawn((DenseVec { x: 120, y: 1 }, ChildOf(root))).id();

            let bystander_root = world.spawn(DenseVec { x: 900, y: 0 }).id();
            let bystander_child = world
                .spawn((DenseVec { x: 910, y: 1 }, ChildOf(bystander_root)))
                .id();

            let doomed = vec![root, c1, gc1, gc2, c2];
            let survivors = vec![bystander_root, bystander_child];

            if attach_dynamic {
                let reg = world.resource::<ModRegistry>().clone();
                let mut q = CommandQueue::default();
                let mut bridge = CommandQueueBridge::new(&mut q, world.entity_allocator(), &reg);
                let raw_q = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
                for (idx, &e) in doomed.iter().chain(survivors.iter()).enumerate() {
                    let ffi_e = FfiEntity::from_bits(e.to_bits());
                    let pod_bytes = [idx as u8; 16];
                    let drop_bytes = (idx as u64).to_ne_bytes();
                    // SAFETY: Valid `raw_q` and exact byte sizes.
                    unsafe {
                        assert_eq!(
                            host_cmd_insert_component(
                                raw_q,
                                ffi_e,
                                c"DynPodStat".as_ptr(),
                                pod_bytes.as_ptr(),
                                16
                            ),
                            FfiResult::OK
                        );
                        assert_eq!(
                            host_cmd_insert_component(
                                raw_q,
                                ffi_e,
                                c"DynDropRes".as_ptr(),
                                drop_bytes.as_ptr(),
                                8
                            ),
                            FfiResult::OK
                        );
                    }
                }
                q.apply(world);
            }

            (root, doomed, survivors)
        };

    let (root_a, doomed_a, survivors_a) = spawn_trees(app_native.world_mut(), false);
    let (root_b, doomed_b, survivors_b) = spawn_trees(app_dynamic.world_mut(), true);
    assert_eq!(HIERARCHY_DYN_DROPS.load(Ordering::SeqCst), 0);

    // Despawn `root_a` via native `Commands` and `root_b` via `DynamicCSystem` (`host_cmd_despawn`)
    app_native.add_systems(Update, move |mut commands: Commands| {
        commands.entity(root_a).despawn();
    });

    let root_b_ffi = Box::new(FfiEntity::from_bits(root_b.to_bits()));
    let reg_b = Arc::new(app_dynamic.world().resource::<ModRegistry>().clone());
    let dyn_despawn = DynamicCSystem::new(
        "dyn_despawn_hierarchy_root".to_string(),
        vec![],
        despawn_target_entity_cb,
        (&*root_b_ffi as *const FfiEntity) as *mut c_void,
        reg_b,
    );
    app_dynamic.add_systems(Update, dyn_despawn);

    app_native.update();
    app_dynamic.update();

    // Every node in the 5-entity `ChildOf` tree must be recursively despawned in BOTH worlds!
    for e in &doomed_a {
        assert!(app_native.world().get_entity(*e).is_err());
    }
    for e in &doomed_b {
        assert!(app_dynamic.world().get_entity(*e).is_err());
    }
    for e in &survivors_a {
        assert!(app_native.world().get_entity(*e).is_ok());
    }
    for e in &survivors_b {
        let er = app_dynamic.world().entity(*e);
        assert!(er.contains_id(dyn_pod_id));
        assert!(er.contains_id(dyn_drop_id));
    }

    assert_eq!(
        snapshot_world(app_native.world_mut()),
        snapshot_world(app_dynamic.world_mut()),
        "Post-hierarchy-despawn survivor snapshot must match identically"
    );
    assert_eq!(
        HIERARCHY_DYN_DROPS.load(Ordering::SeqCst),
        5,
        "Recursive ChildOf despawn must invoke foreign drop_fn once for each of the 5 tree nodes"
    );

    // Dropping World B must drop the remaining 2 bystander entities (5 + 2 = 7 total drops)
    drop(app_dynamic);
    assert_eq!(HIERARCHY_DYN_DROPS.load(Ordering::SeqCst), 7);

    // --- Part B: `EntityCloner` (`clone_and_spawn`) with POD & Droppable Dynamic Components ---
    let mut clone_world = World::new();
    clone_world.insert_resource(ModRegistry::new());
    let dv_cid = clone_world.register_component::<DenseVec>();
    let oa_cid = clone_world.register_component::<OverAligned32>();
    {
        let mut r = clone_world.resource_mut::<ModRegistry>();
        r.register_component::<DenseVec>(dv_cid);
        r.register_component::<OverAligned32>(oa_cid);
    }

    let (pod_cid, drop_cid) = {
        let reg = Arc::new(clone_world.resource::<ModRegistry>().clone());
        let active = Arc::new(AtomicBool::new(true));
        let mut ctx = ModInitContext::with_world(reg, active, &mut clone_world as *mut World);
        let _g = ctx.make_active();
        let ctx_ptr = (&mut ctx as *mut ModInitContext).cast::<c_void>();

        let pod_desc = ModComponentDescriptor::new(
            c"ClonerPodComp".as_ptr(),
            ComponentUuid::from_name("ClonerPodComp"),
            16,
            8,
        );
        let drop_desc = ModComponentDescriptor::new(
            c"ClonerDropComp".as_ptr(),
            ComponentUuid::from_name("ClonerDropComp"),
            8,
            8,
        )
        .with_drop(count_cloner_drop);

        // SAFETY: Valid `ctx_ptr` and descriptors.
        unsafe {
            assert_eq!(host_register_component(ctx_ptr, &pod_desc), FfiResult::OK);
            assert_eq!(host_register_component(ctx_ptr, &drop_desc), FfiResult::OK);
        }
        drop(_g);

        let r = clone_world.resource::<ModRegistry>();
        (
            r.get_by_name("ClonerPodComp").unwrap().component_id,
            r.get_by_name("ClonerDropComp").unwrap().component_id,
        )
    };

    assert!(matches!(
        clone_world
            .components()
            .get_info(pod_cid)
            .unwrap()
            .clone_behavior(),
        ComponentCloneBehavior::Default
    ));
    assert!(
        matches!(
            clone_world
                .components()
                .get_info(drop_cid)
                .unwrap()
                .clone_behavior(),
            ComponentCloneBehavior::Ignore
        ),
        "Droppable dynamic components must use ComponentCloneBehavior::Ignore"
    );

    let src = clone_world
        .spawn((
            DenseVec { x: 42, y: -99 },
            OverAligned32 {
                lanes: [10, 20, 30, 40],
            },
        ))
        .id();
    {
        let reg = clone_world.resource::<ModRegistry>().clone();
        let mut q = CommandQueue::default();
        let mut bridge = CommandQueueBridge::new(&mut q, clone_world.entity_allocator(), &reg);
        let raw_q = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
        let src_ffi = FfiEntity::from_bits(src.to_bits());
        // SAFETY: Valid `raw_q` and exact byte lengths.
        unsafe {
            assert_eq!(
                host_cmd_insert_component(
                    raw_q,
                    src_ffi,
                    c"ClonerPodComp".as_ptr(),
                    [0x77u8; 16].as_ptr(),
                    16
                ),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_component(
                    raw_q,
                    src_ffi,
                    c"ClonerDropComp".as_ptr(),
                    [0x88u8; 8].as_ptr(),
                    8
                ),
                FfiResult::OK
            );
        }
        q.apply(&mut clone_world);
    }

    let cloned = clone_world.entity_mut(src).clone_and_spawn();
    assert_eq!(
        clone_world.get::<DenseVec>(cloned),
        Some(&DenseVec { x: 42, y: -99 })
    );
    assert_eq!(
        clone_world.get::<OverAligned32>(cloned),
        Some(&OverAligned32 {
            lanes: [10, 20, 30, 40]
        })
    );
    assert!(
        !clone_world.entity(cloned).contains_id(drop_cid),
        "ComponentCloneBehavior::Ignore must prevent bit-copying foreign-droppable component"
    );

    clone_world.despawn(cloned);
    assert_eq!(
        CLONER_DYN_DROPS.load(Ordering::SeqCst),
        0,
        "Despawning cloned entity must NOT run foreign drop_fn"
    );

    clone_world.despawn(src);
    assert_eq!(
        CLONER_DYN_DROPS.load(Ordering::SeqCst),
        1,
        "Despawning original entity must run foreign drop_fn exactly once"
    );
}

// ============================================================================
// Suite 4: Concurrent Multi-World `std::thread::scope` Executor & Drop Isolation
// ============================================================================

const MAX_PARALLEL_WORLDS: usize = 6;
static PARALLEL_WORLD_DROPS: [AtomicUsize; MAX_PARALLEL_WORLDS] = [
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
    AtomicUsize::new(0),
];

unsafe extern "C" fn drop_parallel_world_0(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[0].fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn drop_parallel_world_1(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[1].fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn drop_parallel_world_2(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[2].fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn drop_parallel_world_3(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[3].fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn drop_parallel_world_4(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[4].fetch_add(1, Ordering::SeqCst);
}
unsafe extern "C" fn drop_parallel_world_5(_: *mut c_void) {
    PARALLEL_WORLD_DROPS[5].fetch_add(1, Ordering::SeqCst);
}

const PARALLEL_DROP_FNS: [unsafe extern "C" fn(*mut c_void); MAX_PARALLEL_WORLDS] = [
    drop_parallel_world_0,
    drop_parallel_world_1,
    drop_parallel_world_2,
    drop_parallel_world_3,
    drop_parallel_world_4,
    drop_parallel_world_5,
];

struct ParallelWorldProbe {
    expected_sym_value: usize,
    foreign_sym_cstr: std::ffi::CString,
    failed: AtomicBool,
    runs: AtomicUsize,
}

unsafe extern "C" fn verify_worker_isolation_and_mutate_dense(
    iter: *mut RawChunkIter,
    _commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: `user_data` points to `ParallelWorldProbe` alive for the scope of the world's thread.
    let probe = unsafe { &*(user_data as *const ParallelWorldProbe) };

    // SAFETY: Valid NUL-terminated C strings.
    let own = unsafe { host_get_proc_address(c"OwnScopedWorldSym".as_ptr()) } as usize;
    let poison = unsafe { host_get_proc_address(c"GlobalPoisonOnlySym".as_ptr()) };
    let other_world = unsafe { host_get_proc_address(probe.foreign_sym_cstr.as_ptr()) };

    if own != probe.expected_sym_value || !poison.is_null() || !other_world.is_null() {
        probe.failed.store(true, Ordering::SeqCst);
        return FfiResult::INTERNAL_ERROR;
    }

    loop {
        // SAFETY: Valid `iter`.
        let count = unsafe { host_iter_next_chunk(iter) };
        if count == 0 {
            break;
        }
        // SAFETY: Col 0 is WRITE (either DenseVec or SparseEnergy, both start with an `i64` field!).
        let ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut i64;
        if ptr.is_null() {
            probe.failed.store(true, Ordering::SeqCst);
            return FfiResult::NULL_POINTER;
        }
        for r in 0..count {
            // SAFETY: `r < count`.
            unsafe {
                *ptr.add(r) = (*ptr.add(r)).wrapping_add(1);
            }
        }
    }

    probe.runs.fetch_add(1, Ordering::SeqCst);
    FfiResult::OK
}

#[test]
fn differential_multi_world_parallel_thread_scope_executor_and_drop_isolation() {
    #[cfg(not(miri))]
    const ACTIVE_WORLDS: usize = 6;
    #[cfg(miri)]
    const ACTIVE_WORLDS: usize = 2;

    const ENTITIES_PER_WORLD: usize = 12;

    for slot in &PARALLEL_WORLD_DROPS {
        slot.store(0, Ordering::SeqCst);
    }

    // Pollute GLOBAL_SYMBOLS before spawning concurrent Worlds to ensure Bevy worker threads
    // NEVER fall back to GLOBAL_SYMBOLS when `CURRENT_REGISTRY` is bound by `DynamicCSystem::run_unsafe`!
    let mut poison_reg = ModRegistry::new();
    poison_reg.insert_symbol(
        "GlobalPoisonOnlySym",
        core::ptr::without_provenance(0xDEAD_BEEF),
    );
    for idx in 0..ACTIVE_WORLDS {
        poison_reg.insert_symbol(
            format!("WorldPrivateSym_{idx}"),
            core::ptr::without_provenance(0x9000 + idx),
        );
    }

    let shared_drop_uuid = ComponentUuid::from_name("ConcurrentMultiWorldDropComp");

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ACTIVE_WORLDS);

        for w_idx in 0..ACTIVE_WORLDS {
            handles.push(scope.spawn(move || {
                let mut app = App::new();
                app.add_plugins(AbiModPlugin);
                register_full_schema(&mut app);

                let expected_sym = 0x50000usize + w_idx * 0x100 + 7;
                {
                    let mut reg = app.world_mut().resource_mut::<ModRegistry>();
                    reg.insert_symbol(
                        "OwnScopedWorldSym",
                        core::ptr::without_provenance(expected_sym),
                    );
                    reg.insert_symbol(
                        format!("WorldPrivateSym_{w_idx}"),
                        core::ptr::without_provenance(expected_sym),
                    );
                }

                // Register the SAME ComponentUuid with a world-specific foreign `drop_fn`!
                {
                    let world = app.world_mut();
                    let reg_arc = Arc::new(world.resource::<ModRegistry>().clone());
                    let active = Arc::new(AtomicBool::new(true));
                    let mut ctx = ModInitContext::with_world(reg_arc, active, world as *mut World);
                    let _g = ctx.make_active();
                    let desc = ModComponentDescriptor::new(
                        c"ConcurrentMultiWorldDropComp".as_ptr(),
                        shared_drop_uuid,
                        8,
                        8,
                    )
                    .with_drop(PARALLEL_DROP_FNS[w_idx]);
                    // SAFETY: Valid `ModInitContext` and descriptor.
                    assert_eq!(
                        unsafe {
                            host_register_component(
                                (&mut ctx as *mut ModInitContext).cast::<c_void>(),
                                &desc,
                            )
                        },
                        FfiResult::OK
                    );
                }

                // Spawn `ENTITIES_PER_WORLD` entities carrying DenseVec, SparseEnergy, SparseBadge, and the droppable dynamic component
                let mut spawned = Vec::with_capacity(ENTITIES_PER_WORLD);
                for i in 0..ENTITIES_PER_WORLD {
                    let mut emut = app.world_mut().spawn((
                        DenseVec {
                            x: (w_idx * 100 + i) as i64,
                            y: i as i64,
                        },
                        SparseEnergy { energy: 10 },
                    ));
                    if i % 2 == 0 {
                        emut.insert(SparseBadge { tier: 1 });
                    }
                    spawned.push(emut.id());
                }

                {
                    let world = app.world_mut();
                    let reg = world.resource::<ModRegistry>().clone();
                    let mut q = CommandQueue::default();
                    let mut bridge = CommandQueueBridge::new(&mut q, world.entity_allocator(), &reg);
                    let raw_q = (&mut bridge as *mut CommandQueueBridge<'_>).cast::<RawCommandQueue>();
                    for &e in &spawned {
                        let bytes = (w_idx as u64).to_ne_bytes();
                        // SAFETY: Valid `raw_q` and 8-byte payload.
                        assert_eq!(
                            unsafe {
                                host_cmd_insert_component(
                                    raw_q,
                                    FfiEntity::from_bits(e.to_bits()),
                                    c"ConcurrentMultiWorldDropComp".as_ptr(),
                                    bytes.as_ptr(),
                                    8,
                                )
                            },
                            FfiResult::OK
                        );
                    }
                    q.apply(world);
                }

                let neighbor_idx = (w_idx + 1) % ACTIVE_WORLDS;
                let probe = Arc::new(ParallelWorldProbe {
                    expected_sym_value: expected_sym,
                    foreign_sym_cstr: std::ffi::CString::new(format!(
                        "WorldPrivateSym_{neighbor_idx}"
                    ))
                    .unwrap(),
                    failed: AtomicBool::new(false),
                    runs: AtomicUsize::new(0),
                });

                let reg_arc = Arc::new(app.world().resource::<ModRegistry>().clone());
                let ud = Arc::as_ptr(&probe) as *mut c_void;

                // Register 3 mutually disjoint DynamicCSystems so Bevy's MultiThreaded executor
                // runs them concurrently across worker threads!
                let sys_dense_with = DynamicCSystem::new(
                    format!("w{w_idx}_dense_with_badge"),
                    vec![
                        ComponentAccessDescOwned {
                            name: "DenseVec".to_string(),
                            mode: ComponentAccessMode::WRITE,
                        },
                        ComponentAccessDescOwned {
                            name: "SparseBadge".to_string(),
                            mode: ComponentAccessMode::FILTER_ONLY,
                        },
                    ],
                    verify_worker_isolation_and_mutate_dense,
                    ud,
                    Arc::clone(&reg_arc),
                );
                let sys_dense_without = DynamicCSystem::new(
                    format!("w{w_idx}_dense_without_badge"),
                    vec![
                        ComponentAccessDescOwned {
                            name: "DenseVec".to_string(),
                            mode: ComponentAccessMode::WRITE,
                        },
                        ComponentAccessDescOwned {
                            name: "SparseBadge".to_string(),
                            mode: ComponentAccessMode::WITHOUT,
                        },
                    ],
                    verify_worker_isolation_and_mutate_dense,
                    ud,
                    Arc::clone(&reg_arc),
                );
                let sys_sparse = DynamicCSystem::new(
                    format!("w{w_idx}_sparse_energy"),
                    vec![ComponentAccessDescOwned {
                        name: "SparseEnergy".to_string(),
                        mode: ComponentAccessMode::WRITE,
                    }],
                    verify_worker_isolation_and_mutate_dense,
                    ud,
                    Arc::clone(&reg_arc),
                );

                app.add_systems(Update, (sys_dense_with, sys_dense_without, sys_sparse));

                for frame in 0..5 {
                    app.update();
                    if frame == 2 {
                        // Despawn the first 4 entities mid-flight; remaining 8 will drop when `app` drops!
                        for &e in &spawned[..4] {
                            app.world_mut().despawn(e);
                        }
                        assert_eq!(
                            PARALLEL_WORLD_DROPS[w_idx].load(Ordering::SeqCst),
                            4,
                            "Mid-flight despawn in world {w_idx} must invoke only world {w_idx}'s drop_fn"
                        );
                    }
                }

                assert!(
                    !probe.failed.load(Ordering::SeqCst),
                    "Worker-thread host_get_proc_address isolation failed in world {w_idx}"
                );
                assert_eq!(probe.runs.load(Ordering::SeqCst), 15);

                for &e in &spawned[4..] {
                    assert_eq!(app.world().get::<SparseEnergy>(e).unwrap().energy, 15);
                }

                // Drop `app` directly to exercise `World::drop` (`Table::drop`) concurrently across threads!
                drop(app);
            }));
        }

        for h in handles {
            h.join().expect("parallel world thread panicked");
        }
    });

    for (w_idx, slot) in PARALLEL_WORLD_DROPS.iter().enumerate().take(ACTIVE_WORLDS) {
        assert_eq!(
            slot.load(Ordering::SeqCst),
            ENTITIES_PER_WORLD,
            "World {w_idx} must observe exact isolated drop count ({ENTITIES_PER_WORLD})"
        );
    }
}
