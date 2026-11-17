//! Differential Native-vs-Dynamic Bevy ECS Parity, `proptest!` Suite & Multi-World Isolation (`ABI 0.2.0`).

use bevy_abi_mod_host::{AbiModPlugin, ComponentOptions, ModAppExt, ModRegistry};
use bevy_abi_mod_host_sys::{
    BamQuerySpecOwned, GuestSystem, HostCommands, HostCommandsStorage, LoadContext, WorldDropGuard,
    host_cmd_despawn, host_cmd_insert, host_cmd_spawn, host_get_proc_address,
    host_iter_column_mutable, host_iter_column_mutable_untracked, host_iter_column_readonly,
    host_iter_column_rows_mutable, host_iter_column_rows_readonly, host_iter_entities,
    host_iter_mark_changed_range, host_iter_next_chunk, host_iter_reset, host_register_component,
    host_res_mut_ptr, host_res_ptr, host_sys_commands, host_sys_host_ctx, host_sys_query,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BamCommands, BamComponent, BamIter, BamSystemCtx, BamTermDesc, ComponentDesc,
    ComponentUuid, FfiEntity, FfiResult,
};
use bevy_app::{App, Update};
use bevy_ecs::change_detection::{DetectChangesMut, MAX_CHANGE_AGE, Tick};
use bevy_ecs::component::{Component, ComponentCloneBehavior, ComponentId};
use bevy_ecs::entity::Entity;
use bevy_ecs::hierarchy::ChildOf;
use bevy_ecs::query::{Changed, FilteredAccessSet, QueryBuilder, With, Without};
use bevy_ecs::resource::Resource;
use bevy_ecs::schedule::{IntoScheduleConfigs, LogLevel, Schedule, ScheduleBuildSettings};
use bevy_ecs::system::{Commands, IntoSystem, Query, System, SystemAccess};
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

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

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[mod_component(name = "diff_parity::DenseVec")]
pub struct DenseVec {
    pub x: i64,
    pub y: i64,
}

#[repr(C, align(32))]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[mod_component(name = "diff_parity::OverAligned32")]
pub struct OverAligned32 {
    pub lanes: [i64; 4],
}

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq)]
#[mod_component(name = "diff_parity::DenseZst")]
pub struct DenseZst;

#[repr(C, align(16))]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq)]
#[mod_component(name = "diff_parity::Aligned16Zst")]
pub struct Aligned16Zst;

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[component(storage = "SparseSet")]
#[mod_component(name = "diff_parity::SparseEnergy")]
pub struct SparseEnergy {
    pub energy: i64,
}

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[component(storage = "SparseSet")]
#[mod_component(name = "diff_parity::SparseBadge")]
pub struct SparseBadge {
    pub tier: u32,
}

#[repr(C)]
#[derive(Component, ModComponent, Debug, Clone, Copy, PartialEq, Eq)]
#[component(storage = "SparseSet")]
#[mod_component(name = "diff_parity::SparseZst")]
pub struct SparseZst;

#[repr(C)]
#[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[mod_component(name = "diff_parity::GlobalBiasRes")]
pub struct GlobalBiasRes {
    pub delta: i64,
    pub writes: u32,
}

#[repr(C)]
#[derive(Resource, ModComponent, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[mod_component(name = "diff_parity::GlobalFlagRes")]
pub struct GlobalFlagRes {
    pub mask: u32,
}

#[derive(Clone, Copy, Debug)]
struct SchemaHandles {
    dense_vec: BamComponent,
    over_aligned: BamComponent,
    dense_zst: BamComponent,
    aligned16_zst: BamComponent,
    sparse_energy: BamComponent,
    sparse_badge: BamComponent,
    sparse_zst: BamComponent,
    bias_res: BamComponent,
    flag_res: BamComponent,
}

impl SchemaHandles {
    fn entity_handles(&self) -> [BamComponent; 7] {
        [
            self.dense_vec,
            self.over_aligned,
            self.dense_zst,
            self.aligned16_zst,
            self.sparse_energy,
            self.sparse_badge,
            self.sparse_zst,
        ]
    }

    fn resource_handles(&self) -> [BamComponent; 2] {
        [self.bias_res, self.flag_res]
    }
}

fn register_full_schema(app: &mut App) -> SchemaHandles {
    let dense_vec = app
        .register_mod_component::<DenseVec>(ComponentOptions::new())
        .unwrap();
    let over_aligned = app
        .register_mod_component::<OverAligned32>(ComponentOptions::new())
        .unwrap();
    let dense_zst = app
        .register_mod_component::<DenseZst>(ComponentOptions::new())
        .unwrap();
    let aligned16_zst = app
        .register_mod_component::<Aligned16Zst>(ComponentOptions::new())
        .unwrap();
    let sparse_energy = app
        .register_mod_component::<SparseEnergy>(ComponentOptions::new())
        .unwrap();
    let sparse_badge = app
        .register_mod_component::<SparseBadge>(ComponentOptions::new())
        .unwrap();
    let sparse_zst = app
        .register_mod_component::<SparseZst>(ComponentOptions::new())
        .unwrap();
    let bias_res = app.register_mod_resource::<GlobalBiasRes>().unwrap();
    let flag_res = app.register_mod_resource::<GlobalFlagRes>().unwrap();

    app.insert_resource(GlobalBiasRes {
        delta: 3,
        writes: 0,
    });
    app.insert_resource(GlobalFlagRes { mask: 0xA5A5 });

    SchemaHandles {
        dense_vec,
        over_aligned,
        dense_zst,
        aligned16_zst,
        sparse_energy,
        sparse_badge,
        sparse_zst,
        bias_res,
        flag_res,
    }
}

unsafe extern "C" fn noop_cb(_sys_ctx: *mut BamSystemCtx, _user_data: *mut c_void) -> FfiResult {
    FfiResult::OK
}

fn build_native_reference_access(
    world: &mut World,
    registry: &ModRegistry,
    groups: &[BamQuerySpecOwned],
) -> (bool, FilteredAccessSet) {
    let mut combined = FilteredAccessSet::new();
    let mut res_reads = BTreeSet::new();
    let mut res_writes = BTreeSet::new();
    let mut res_order = Vec::new();

    for group in groups {
        let mut seen_read = BTreeSet::new();
        let mut seen_write = BTreeSet::new();
        let mut has_entity_terms = false;
        let mut resolved = Vec::with_capacity(group.terms.len());

        for term in &group.terms {
            let meta = registry
                .get_by_handle(term.component)
                .expect("registered schema handle");
            let comp_id = meta.component_id;
            if term.mode.is_resource() != meta.is_resource() {
                return (true, FilteredAccessSet::new());
            }

            match term.mode {
                AccessMode::READ | AccessMode::OPTIONAL_READ => {
                    has_entity_terms = true;
                    if seen_write.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    seen_read.insert(comp_id);
                }
                AccessMode::WRITE | AccessMode::OPTIONAL_WRITE => {
                    has_entity_terms = true;
                    if seen_read.contains(&comp_id) || seen_write.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    seen_write.insert(comp_id);
                }
                AccessMode::FILTER_ONLY
                | AccessMode::WITHOUT
                | AccessMode::CHANGED
                | AccessMode::ADDED => {
                    has_entity_terms = true;
                }
                AccessMode::RESOURCE_READ => {
                    if res_writes.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    if res_reads.insert(comp_id) {
                        res_order.push(comp_id);
                    }
                }
                AccessMode::RESOURCE_WRITE => {
                    if res_reads.contains(&comp_id) || res_writes.contains(&comp_id) {
                        return (true, FilteredAccessSet::new());
                    }
                    res_writes.insert(comp_id);
                    res_order.push(comp_id);
                }
                _ => unreachable!("valid mode generated"),
            }
            resolved.push((comp_id, term.mode));
        }

        if group.terms.is_empty() || has_entity_terms {
            let mut builder = QueryBuilder::<()>::new(world);
            for &(comp_id, mode) in &resolved {
                match mode {
                    AccessMode::READ => {
                        builder.ref_id(comp_id);
                    }
                    AccessMode::WRITE => {
                        builder.mut_id(comp_id);
                    }
                    AccessMode::OPTIONAL_READ => {
                        builder.optional(|b| {
                            b.ref_id(comp_id);
                        });
                    }
                    AccessMode::OPTIONAL_WRITE => {
                        builder.optional(|b| {
                            b.mut_id(comp_id);
                        });
                    }
                    AccessMode::FILTER_ONLY => {
                        builder.with_id(comp_id);
                    }
                    AccessMode::WITHOUT => {
                        builder.without_id(comp_id);
                    }
                    AccessMode::CHANGED | AccessMode::ADDED => {
                        if seen_write.contains(&comp_id) {
                            builder.with_id(comp_id);
                        } else {
                            builder.ref_id(comp_id);
                        }
                    }
                    _ => {}
                }
            }
            let state = builder.build();
            let fa = state.component_access().clone();
            let mut candidate = FilteredAccessSet::new();
            candidate.add(fa.clone());
            if !combined.is_compatible(&candidate) {
                return (true, FilteredAccessSet::new());
            }
            combined.add(fa);
        }
    }

    for res_id in res_order {
        let writable = res_writes.contains(&res_id);
        let mut candidate = FilteredAccessSet::new();
        if writable {
            candidate.add_resource_write(res_id);
        } else {
            candidate.add_resource_read(res_id);
        }
        if !combined.is_compatible(&candidate) {
            return (true, FilteredAccessSet::new());
        }
        if writable {
            combined.add_resource_write(res_id);
        } else {
            combined.add_resource_read(res_id);
        }
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
    let schema = register_full_schema(&mut app);
    let entity_handles = schema.entity_handles();

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

    let reg = app.world().resource::<ModRegistry>().clone();
    let modes = [
        AccessMode::READ,
        AccessMode::WRITE,
        AccessMode::FILTER_ONLY,
        AccessMode::WITHOUT,
        AccessMode::OPTIONAL_READ,
        AccessMode::OPTIONAL_WRITE,
        AccessMode::CHANGED,
        AccessMode::ADDED,
    ];

    let mut rng = Rng::new(0xA11C_E550_2026_0001);
    let mut valid_dyn_sets: Vec<(Vec<BamQuerySpecOwned>, FilteredAccessSet)> = Vec::new();
    let mut valid_native_sets: Vec<FilteredAccessSet> = Vec::new();
    let mut observed_self_conflicts = 0usize;

    for idx in 0..NUM_SPECS {
        let group_count = if idx % 3 == 0 { 1 } else { 1 + rng.range(4) };
        let mut groups = Vec::with_capacity(group_count);
        for _ in 0..group_count {
            let clause_count = 1 + rng.range(5);
            let mut terms = Vec::with_capacity(clause_count);
            for _ in 0..clause_count {
                if rng.range(8) == 0 {
                    let res_handles = schema.resource_handles();
                    let rh = res_handles[rng.range(res_handles.len())];
                    let rmode = if rng.gen_bool() {
                        AccessMode::RESOURCE_READ
                    } else {
                        AccessMode::RESOURCE_WRITE
                    };
                    terms.push(BamTermDesc::new(rh, rmode));
                } else {
                    terms.push(BamTermDesc::new(
                        entity_handles[rng.range(entity_handles.len())],
                        modes[rng.range(modes.len())],
                    ));
                }
            }
            groups.push(BamQuerySpecOwned { flags: 0, terms });
        }

        let mut dyn_sys = GuestSystem::new(
            format!("gen_sys_{idx}"),
            groups.clone(),
            noop_cb,
            core::ptr::null_mut(),
            reg.clone(),
        );

        let dyn_sys_access = System::initialize(&mut dyn_sys, app.world_mut());
        let (native_conflict, native_set) =
            build_native_reference_access(app.world_mut(), &reg, &groups);

        assert_eq!(dyn_sys.has_self_conflict(), native_conflict);
        assert_eq!(dyn_sys.is_enabled(), !native_conflict);

        if native_conflict {
            observed_self_conflicts += 1;
            assert!(matches!(dyn_sys_access, SystemAccess::None));
        } else {
            let SystemAccess::Shared(dyn_set) = dyn_sys_access else {
                panic!("Valid GuestSystem must return SystemAccess::Shared");
            };
            valid_dyn_sets.push((groups, dyn_set));
            valid_native_sets.push(native_set);
        }
    }

    assert!(observed_self_conflicts > 0 && valid_dyn_sets.len() >= 10);

    let mut compatible_pairs = 0usize;
    let mut conflicting_pairs = 0usize;
    for i in 0..valid_dyn_sets.len() {
        for j in 0..valid_dyn_sets.len() {
            let dyn_compat = valid_dyn_sets[i].1.is_compatible(&valid_dyn_sets[j].1);
            let native_compat = valid_native_sets[i].is_compatible(&valid_native_sets[j]);
            assert_eq!(dyn_compat, native_compat);
            if dyn_compat {
                compatible_pairs += 1;
            } else {
                conflicting_pairs += 1;
            }
        }
    }
    assert!(compatible_pairs > 0 && conflicting_pairs > 0);

    let mut strict_schedule = Schedule::default();
    strict_schedule.set_build_settings(ScheduleBuildSettings {
        ambiguity_detection: LogLevel::Error,
        ..Default::default()
    });

    let dyn_disjoint_a = GuestSystem::new(
        "disjoint_a",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::write(schema.dense_vec),
                BamTermDesc::filter_only(schema.sparse_badge),
            ],
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg.clone(),
    );
    let dyn_disjoint_b = GuestSystem::new(
        "disjoint_b",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::write(schema.dense_vec),
                BamTermDesc::without(schema.sparse_badge),
            ],
        }],
        noop_cb,
        core::ptr::null_mut(),
        reg,
    );
    strict_schedule.add_systems((dyn_disjoint_a, dyn_disjoint_b));
    strict_schedule
        .initialize(app.world_mut())
        .expect("Disjoint SparseSet With/Without GuestSystems must have zero ambiguity");
}

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
    schema: SchemaHandles,
}

unsafe extern "C" fn dyn_stage1_dense_without_sparse_zst(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    let mut iter: *mut BamIter = core::ptr::null_mut();
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
        FfiResult::OK
    );
    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        let dv_ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut DenseVec;
        let oa_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const OverAligned32;
        assert!(!dv_ptr.is_null() && !oa_ptr.is_null());
        assert_eq!(
            (oa_ptr as usize) % core::mem::align_of::<OverAligned32>(),
            0
        );
        for row in 0..count {
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

unsafe extern "C" fn dyn_stage2_sparse_data_rw(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    let mut iter: *mut BamIter = core::ptr::null_mut();
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
        FfiResult::OK
    );
    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        // `SparseEnergy` uses indirect per-row mutable pointers!
        let se_rows = unsafe { host_iter_column_rows_mutable(iter, 0) };
        let dv_ptr = unsafe { host_iter_column_readonly(iter, 1) } as *const DenseVec;
        assert!(!se_rows.is_null() && !dv_ptr.is_null());
        for row in 0..count {
            unsafe {
                let se_p = (*se_rows.add(row)) as *mut SparseEnergy;
                (*se_p).energy = (*se_p).energy.wrapping_add((*dv_ptr.add(row)).x & 0xFF);
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn dyn_stage3_multi_query_nested(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }

    let mut q_src: *mut BamIter = core::ptr::null_mut();
    let mut q_dst: *mut BamIter = core::ptr::null_mut();
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 0, &mut q_src) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 1, &mut q_dst) },
        FfiResult::OK
    );

    let mut sum_lane2: i64 = 0;
    let mut c = 0usize;
    while unsafe { host_iter_next_chunk(q_src, &mut c) }.get() {
        let ptr = unsafe { host_iter_column_readonly(q_src, 0) } as *const OverAligned32;
        for r in 0..c {
            sum_lane2 = sum_lane2.wrapping_add(unsafe { (*ptr.add(r)).lanes[2] });
        }
    }

    assert_eq!(unsafe { host_iter_reset(q_dst) }, FfiResult::OK);
    while unsafe { host_iter_next_chunk(q_dst, &mut c) }.get() {
        let ptr = unsafe { host_iter_column_mutable(q_dst, 0) } as *mut OverAligned32;
        for r in 0..c {
            unsafe {
                (*ptr.add(r)).lanes[3] = (*ptr.add(r)).lanes[3].wrapping_add(sum_lane2);
            }
        }
    }
    FfiResult::OK
}

unsafe extern "C" fn dyn_stage4_commands(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let ctrl = unsafe { &*(user_data as *const PipelineControl) };
    if !ctrl.mutate_enabled.load(Ordering::SeqCst) {
        return FfiResult::OK;
    }
    let frame = ctrl.frame_number.load(Ordering::SeqCst) as i64;

    let mut iter: *mut BamIter = core::ptr::null_mut();
    let mut commands: *mut BamCommands = core::ptr::null_mut();
    assert_eq!(
        unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
        FfiResult::OK
    );
    assert_eq!(
        unsafe { host_sys_commands(sys_ctx, &mut commands) },
        FfiResult::OK
    );

    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        let se_rows = unsafe { host_iter_column_rows_readonly(iter, 0) };
        let ents = unsafe { host_iter_entities(iter) };
        assert!(!se_rows.is_null() && !ents.is_null());
        for r in 0..count {
            let energy = unsafe { (*((*se_rows.add(r)) as *const SparseEnergy)).energy };
            if energy > 250 {
                let target = unsafe { *ents.add(r) };
                assert_eq!(unsafe { host_cmd_despawn(commands, target) }, FfiResult::OK);
            }
        }
    }

    // Batched `host_cmd_spawn` placing `(DenseVec, OverAligned32, Aligned16Zst)` in a single archetype transition!
    let dv = DenseVec {
        x: 10 + frame,
        y: 20 - frame,
    };
    let oa = OverAligned32 {
        lanes: [1, 1, frame, 0],
    };
    let comps = [
        ctrl.schema.dense_vec,
        ctrl.schema.over_aligned,
        ctrl.schema.aligned16_zst,
    ];
    let ptrs = [
        (&dv as *const DenseVec).cast::<u8>(),
        (&oa as *const OverAligned32).cast::<u8>(),
        core::ptr::null(),
    ];
    let mut spawned = FfiEntity::NULL;
    assert_eq!(
        unsafe { host_cmd_spawn(commands, comps.as_ptr(), ptrs.as_ptr(), 3, &mut spawned) },
        FfiResult::OK
    );
    assert!(!spawned.is_null());

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
                i as i64,
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
    let _schema_nat = register_full_schema(&mut app_native);

    let mut app_dynamic = App::new();
    app_dynamic.add_plugins(AbiModPlugin);
    let schema_dyn = register_full_schema(&mut app_dynamic);

    const SEED: u64 = 0x51DE_B151_DE20_2602;
    seed_world_deterministically(app_native.world_mut(), SEED, INITIAL_ENTITIES);
    seed_world_deterministically(app_dynamic.world_mut(), SEED, INITIAL_ENTITIES);

    assert_eq!(
        snapshot_world(app_native.world_mut()),
        snapshot_world(app_dynamic.world_mut())
    );

    let ctrl = Arc::new(PipelineControl {
        mutate_enabled: AtomicBool::new(true),
        frame_number: AtomicUsize::new(0),
        schema: schema_dyn,
    });

    let report_native = Arc::new(Mutex::new(ChangedReport::default()));
    let report_dynamic = Arc::new(Mutex::new(ChangedReport::default()));

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
        commands.spawn((
            DenseVec {
                x: 10 + frame,
                y: 20 - frame,
            },
            OverAligned32 {
                lanes: [1, 1, frame, 0],
            },
            Aligned16Zst,
        ));
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

    let reg_b = app_dynamic.world().resource::<ModRegistry>().clone();
    let ud = Arc::as_ptr(&ctrl) as *mut c_void;

    let dyn_s1 = GuestSystem::new(
        "dyn_s1",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::write(schema_dyn.dense_vec),
                BamTermDesc::read(schema_dyn.over_aligned),
                BamTermDesc::without(schema_dyn.sparse_zst),
            ],
        }],
        dyn_stage1_dense_without_sparse_zst,
        ud,
        reg_b.clone(),
    );

    let dyn_s2 = GuestSystem::new(
        "dyn_s2",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::write(schema_dyn.sparse_energy),
                BamTermDesc::read(schema_dyn.dense_vec),
                BamTermDesc::filter_only(schema_dyn.sparse_badge),
            ],
        }],
        dyn_stage2_sparse_data_rw,
        ud,
        reg_b.clone(),
    );

    let dyn_s3 = GuestSystem::new(
        "dyn_s3",
        vec![
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::read(schema_dyn.over_aligned),
                    BamTermDesc::filter_only(schema_dyn.dense_zst),
                ],
            },
            BamQuerySpecOwned {
                flags: 0,
                terms: vec![
                    BamTermDesc::write(schema_dyn.over_aligned),
                    BamTermDesc::without(schema_dyn.dense_zst),
                ],
            },
        ],
        dyn_stage3_multi_query_nested,
        ud,
        reg_b.clone(),
    );

    let dyn_s4 = GuestSystem::new(
        "dyn_s4",
        vec![BamQuerySpecOwned {
            flags: 0,
            terms: vec![
                BamTermDesc::read(schema_dyn.sparse_energy),
                BamTermDesc::without(schema_dyn.dense_zst),
            ],
        }],
        dyn_stage4_commands,
        ud,
        reg_b,
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

    for frame in 1..=8usize {
        ctrl.frame_number.store(frame, Ordering::SeqCst);
        let active = frame != 4 && frame != 7;
        ctrl.mutate_enabled.store(active, Ordering::SeqCst);

        app_native.update();
        app_dynamic.update();

        let ch_native = report_native.lock().unwrap().clone();
        let ch_dynamic = report_dynamic.lock().unwrap().clone();
        assert_eq!(ch_native, ch_dynamic);
        if !active {
            assert!(
                ch_dynamic.dense_vec_bits.is_empty()
                    && ch_dynamic.over_aligned_bits.is_empty()
                    && ch_dynamic.sparse_energy_bits.is_empty()
            );
        }

        assert_eq!(
            snapshot_world(app_native.world_mut()),
            snapshot_world(app_dynamic.world_mut())
        );
    }

    #[cfg(not(miri))]
    {
        use bevy_ecs::change_detection::CHECK_TICK_THRESHOLD;
        for app in [&mut app_native, &mut app_dynamic] {
            let world = app.world_mut();
            for _ in 0..CHECK_TICK_THRESHOLD {
                world.increment_change_tick();
            }
            assert!(world.check_change_ticks().is_some());
        }

        for post_scan_step in 1..=3usize {
            ctrl.frame_number
                .store(100 + post_scan_step, Ordering::SeqCst);
            let active = post_scan_step == 2;
            ctrl.mutate_enabled.store(active, Ordering::SeqCst);

            app_native.update();
            app_dynamic.update();

            let ch_native = report_native.lock().unwrap().clone();
            let ch_dynamic = report_dynamic.lock().unwrap().clone();
            assert_eq!(ch_native, ch_dynamic);
            if !active {
                assert!(
                    ch_dynamic.dense_vec_bits.is_empty()
                        && ch_dynamic.over_aligned_bits.is_empty()
                        && ch_dynamic.sparse_energy_bits.is_empty()
                );
            }
            assert_eq!(
                snapshot_world(app_native.world_mut()),
                snapshot_world(app_dynamic.world_mut())
            );
        }
    }

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
        let (dv_h, oa_h) = {
            let mut reg = w_dyn.resource_mut::<ModRegistry>();
            (
                reg.register_component::<DenseVec>(dv_id_d, ComponentOptions::new())
                    .unwrap(),
                reg.register_component::<OverAligned32>(oa_id_d, ComponentOptions::new())
                    .unwrap(),
            )
        };
        w_nat.register_component::<DenseVec>();
        w_nat.register_component::<OverAligned32>();

        let e_nat = w_nat.spawn(DenseVec { x: 7, y: 8 }).id();
        let e_dyn = w_dyn.spawn(DenseVec { x: 7, y: 8 }).id();

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
        let reg_d = w_dyn.resource::<ModRegistry>().clone();
        let ctrl_once = PipelineControl {
            mutate_enabled: AtomicBool::new(true),
            frame_number: AtomicUsize::new(1),
            schema: schema_dyn,
        };
        w_nat.entity_mut(e_nat).insert(OverAligned32 {
            lanes: [3, 0, 0, 0],
        });
        w_dyn.entity_mut(e_dyn).insert(OverAligned32 {
            lanes: [3, 0, 0, 0],
        });

        let mut dyn_sys = GuestSystem::new(
            "wrap_boundary_dyn",
            vec![BamQuerySpecOwned {
                flags: 0,
                terms: vec![BamTermDesc::write(dv_h), BamTermDesc::read(oa_h)],
            }],
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
        assert_eq!(c_nat, c_dyn);
        assert_eq!(w_nat.get::<DenseVec>(e_nat), w_dyn.get::<DenseVec>(e_dyn));
    }
}

// ============================================================================
// Hierarchy ChildOf Despawn, EntityCloner, & Lock-Free Multi-World Isolation
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
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let target = unsafe { *(user_data as *const FfiEntity) };
    let mut commands: *mut BamCommands = core::ptr::null_mut();
    let st = unsafe { host_sys_commands(sys_ctx, &mut commands) };
    if st.is_err() {
        return st;
    }
    unsafe { host_cmd_despawn(commands, target) }
}

#[test]
fn differential_hierarchy_child_of_despawn_and_entity_cloner_drop_parity() {
    HIERARCHY_DYN_DROPS.store(0, Ordering::SeqCst);
    CLONER_DYN_DROPS.store(0, Ordering::SeqCst);

    let mut app_native = App::new();
    app_native.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_native);

    let mut app_dynamic = App::new();
    app_dynamic.add_plugins(AbiModPlugin);
    register_full_schema(&mut app_dynamic);

    let (dyn_pod_h, dyn_drop_h, dyn_pod_id, dyn_drop_id) = {
        let world = app_dynamic.world_mut();
        let reg = world.resource::<ModRegistry>().clone();
        let active = Arc::new(AtomicBool::new(true));
        let mut ctx = LoadContext::with_world_and_mod_id(reg, active, world as *mut World, None);

        let pod_desc = ComponentDesc::new(
            c"diff_parity::DynPodStat".as_ptr(),
            ComponentUuid::from_parts("diff_parity::DynPodStat", 16, 8, 1),
            16,
            8,
        );
        let drop_desc = ComponentDesc::new(
            c"diff_parity::DynDropRes".as_ptr(),
            ComponentUuid::from_parts("diff_parity::DynDropRes", 8, 8, 2),
            8,
            8,
        )
        .with_drop(count_hierarchy_drop);

        let mut pod_h = BamComponent::INVALID;
        let mut drop_h = BamComponent::INVALID;
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &pod_desc, &mut pod_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &drop_desc, &mut drop_h),
                FfiResult::OK
            );
        }
        ctx.finish_init();

        let r = world.resource::<ModRegistry>();
        (
            pod_h,
            drop_h,
            r.get_by_handle(pod_h).unwrap().component_id,
            r.get_by_handle(drop_h).unwrap().component_id,
        )
    };

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
                let snap = world.resource::<ModRegistry>().snapshot_arc();
                let mut storage = HostCommandsStorage::new();
                {
                    let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                    let raw_q = cmds.as_ffi_ptr();
                    for (idx, &e) in doomed.iter().chain(survivors.iter()).enumerate() {
                        let ffi_e = FfiEntity::from_bits(e.to_bits());
                        let pod_bytes = [idx as u8; 16];
                        let drop_bytes = (idx as u64).to_ne_bytes();
                        unsafe {
                            assert_eq!(
                                host_cmd_insert(raw_q, ffi_e, dyn_pod_h, pod_bytes.as_ptr(), 16),
                                FfiResult::OK
                            );
                            assert_eq!(
                                host_cmd_insert(raw_q, ffi_e, dyn_drop_h, drop_bytes.as_ptr(), 8),
                                FfiResult::OK
                            );
                        }
                    }
                }
                storage.apply_to_world(world, &snap);
            }

            (root, doomed, survivors)
        };

    let (root_a, doomed_a, survivors_a) = spawn_trees(app_native.world_mut(), false);
    let (root_b, doomed_b, survivors_b) = spawn_trees(app_dynamic.world_mut(), true);
    assert_eq!(HIERARCHY_DYN_DROPS.load(Ordering::SeqCst), 0);

    app_native.add_systems(Update, move |mut commands: Commands| {
        commands.entity(root_a).despawn();
    });

    let root_b_ffi = Box::new(FfiEntity::from_bits(root_b.to_bits()));
    let reg_b = app_dynamic.world().resource::<ModRegistry>().clone();
    let dyn_despawn = GuestSystem::new(
        "dyn_despawn_hierarchy_root",
        vec![],
        despawn_target_entity_cb,
        (&*root_b_ffi as *const FfiEntity) as *mut c_void,
        reg_b,
    );
    app_dynamic.add_systems(Update, dyn_despawn);

    app_native.update();
    app_dynamic.update();

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
        snapshot_world(app_dynamic.world_mut())
    );
    assert_eq!(HIERARCHY_DYN_DROPS.load(Ordering::SeqCst), 5);

    // Clear remaining entities while active (`7` total drops across doomed + survivors), then
    // verify `WorldDropGuard` cleanly disarms and reclaims the slot when `app_dynamic` drops.
    app_dynamic.world_mut().clear_entities();
    assert_eq!(HIERARCHY_DYN_DROPS.load(Ordering::SeqCst), 7);
    drop(app_dynamic);
    drop(root_b_ffi);

    let mut clone_world = World::new();
    clone_world.insert_resource(WorldDropGuard::new());
    clone_world.insert_resource(ModRegistry::new());
    let dv_cid = clone_world.register_component::<DenseVec>();
    let oa_cid = clone_world.register_component::<OverAligned32>();
    {
        let mut r = clone_world.resource_mut::<ModRegistry>();
        r.register_component::<DenseVec>(dv_cid, ComponentOptions::new())
            .unwrap();
        r.register_component::<OverAligned32>(oa_cid, ComponentOptions::new())
            .unwrap();
    }

    let (pod_h, drop_h, pod_cid, drop_cid) = {
        let reg = clone_world.resource::<ModRegistry>().clone();
        let active = Arc::new(AtomicBool::new(true));
        let mut ctx =
            LoadContext::with_world_and_mod_id(reg, active, &mut clone_world as *mut World, None);

        let pod_desc = ComponentDesc::new(
            c"diff_parity::ClonerPodComp".as_ptr(),
            ComponentUuid::from_parts("diff_parity::ClonerPodComp", 16, 8, 1),
            16,
            8,
        );
        let drop_desc = ComponentDesc::new(
            c"diff_parity::ClonerDropComp".as_ptr(),
            ComponentUuid::from_parts("diff_parity::ClonerDropComp", 8, 8, 2),
            8,
            8,
        )
        .with_drop(count_cloner_drop);

        let mut ph = BamComponent::INVALID;
        let mut dh = BamComponent::INVALID;
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &pod_desc, &mut ph),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &drop_desc, &mut dh),
                FfiResult::OK
            );
        }
        ctx.finish_init();

        let r = clone_world.resource::<ModRegistry>();
        (
            ph,
            dh,
            r.get_by_handle(ph).unwrap().component_id,
            r.get_by_handle(dh).unwrap().component_id,
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
    assert!(matches!(
        clone_world
            .components()
            .get_info(drop_cid)
            .unwrap()
            .clone_behavior(),
        ComponentCloneBehavior::Ignore
    ));

    let src = clone_world
        .spawn((
            DenseVec { x: 42, y: -99 },
            OverAligned32 {
                lanes: [10, 20, 30, 40],
            },
        ))
        .id();
    {
        let snap = clone_world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, clone_world.entity_allocator(), &snap);
            let raw_q = cmds.as_ffi_ptr();
            let src_ffi = FfiEntity::from_bits(src.to_bits());
            unsafe {
                assert_eq!(
                    host_cmd_insert(raw_q, src_ffi, pod_h, [0x77u8; 16].as_ptr(), 16),
                    FfiResult::OK
                );
                assert_eq!(
                    host_cmd_insert(raw_q, src_ffi, drop_h, [0x88u8; 8].as_ptr(), 8),
                    FfiResult::OK
                );
            }
        }
        storage.apply_to_world(&mut clone_world, &snap);
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
    assert!(!clone_world.entity(cloned).contains_id(drop_cid));

    clone_world.despawn(cloned);
    assert_eq!(CLONER_DYN_DROPS.load(Ordering::SeqCst), 0);

    clone_world.despawn(src);
    assert_eq!(CLONER_DYN_DROPS.load(Ordering::SeqCst), 1);
    drop(clone_world);
}

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
    is_sparse_rows: bool,
    failed: AtomicBool,
    runs: AtomicUsize,
}

unsafe extern "C" fn verify_worker_isolation_and_mutate(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let probe = unsafe { &*(user_data as *const ParallelWorldProbe) };
    let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
    if host_ctx.is_null() {
        probe.failed.store(true, Ordering::SeqCst);
        return FfiResult::NULL_POINTER;
    }

    let own =
        unsafe { host_get_proc_address(host_ctx, c"bam.v0.OwnScopedWorldSym".as_ptr()) } as usize;
    let poison = unsafe { host_get_proc_address(host_ctx, c"bam.v0.GlobalPoisonOnlySym".as_ptr()) };
    let other_world = unsafe { host_get_proc_address(host_ctx, probe.foreign_sym_cstr.as_ptr()) };

    if own != probe.expected_sym_value || !poison.is_null() || !other_world.is_null() {
        probe.failed.store(true, Ordering::SeqCst);
        return FfiResult::INTERNAL_ERROR;
    }

    let mut iter: *mut BamIter = core::ptr::null_mut();
    if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_err() {
        probe.failed.store(true, Ordering::SeqCst);
        return FfiResult::INTERNAL_ERROR;
    }

    let mut count = 0usize;
    while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
        if probe.is_sparse_rows {
            let rows = unsafe { host_iter_column_rows_mutable(iter, 0) };
            if rows.is_null() {
                probe.failed.store(true, Ordering::SeqCst);
                return FfiResult::NULL_POINTER;
            }
            for r in 0..count {
                unsafe {
                    let p = (*rows.add(r)) as *mut i64;
                    *p = (*p).wrapping_add(1);
                }
            }
        } else {
            let ptr = unsafe { host_iter_column_mutable(iter, 0) } as *mut DenseVec;
            if ptr.is_null() {
                probe.failed.store(true, Ordering::SeqCst);
                return FfiResult::NULL_POINTER;
            }
            for r in 0..count {
                unsafe {
                    (*ptr.add(r)).x = (*ptr.add(r)).x.wrapping_add(1);
                }
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

    let mut poison_reg = ModRegistry::new();
    unsafe {
        poison_reg.register_custom_symbol(
            "bam.v0.GlobalPoisonOnlySym",
            core::ptr::without_provenance(0xDEAD_BEEF),
        );
        for idx in 0..ACTIVE_WORLDS {
            poison_reg.register_custom_symbol(
                &format!("bam.v0.WorldPrivateSym_{idx}"),
                core::ptr::without_provenance(0x9000 + idx),
            );
        }
    }

    let shared_drop_uuid =
        ComponentUuid::from_parts("diff_parity::ConcurrentMultiWorldDropComp", 8, 8, 77);

    std::thread::scope(|scope| {
        let mut handles = Vec::with_capacity(ACTIVE_WORLDS);

        for w_idx in 0..ACTIVE_WORLDS {
            handles.push(scope.spawn(move || {
                let mut app = App::new();
                app.add_plugins(AbiModPlugin);
                let schema = register_full_schema(&mut app);

                let expected_sym = 0x50000usize + w_idx * 0x100 + 7;
                unsafe {
                    let mut reg = app.world_mut().resource_mut::<ModRegistry>();
                    reg.register_custom_symbol(
                        "bam.v0.OwnScopedWorldSym",
                        core::ptr::without_provenance(expected_sym),
                    );
                    reg.register_custom_symbol(
                        &format!("bam.v0.WorldPrivateSym_{w_idx}"),
                        core::ptr::without_provenance(expected_sym),
                    );
                }

                let drop_comp_h = {
                    let world = app.world_mut();
                    let reg = world.resource::<ModRegistry>().clone();
                    let active = Arc::new(AtomicBool::new(true));
                    let mut ctx =
                        LoadContext::with_world_and_mod_id(reg, active, world as *mut World, None);
                    let desc = ComponentDesc::new(
                        c"diff_parity::ConcurrentMultiWorldDropComp".as_ptr(),
                        shared_drop_uuid,
                        8,
                        8,
                    )
                    .with_drop(PARALLEL_DROP_FNS[w_idx]);
                    let mut h = BamComponent::INVALID;
                    assert_eq!(
                        unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) },
                        FfiResult::OK
                    );
                    ctx.finish_init();
                    h
                };

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
                    let snap = world.resource::<ModRegistry>().snapshot_arc();
                    let mut storage = HostCommandsStorage::new();
                    {
                        let mut cmds =
                            HostCommands::new(&mut storage, world.entity_allocator(), &snap);
                        let raw_q = cmds.as_ffi_ptr();
                        for &e in &spawned {
                            let bytes = (w_idx as u64).to_ne_bytes();
                            assert_eq!(
                                unsafe {
                                    host_cmd_insert(
                                        raw_q,
                                        FfiEntity::from_bits(e.to_bits()),
                                        drop_comp_h,
                                        bytes.as_ptr(),
                                        8,
                                    )
                                },
                                FfiResult::OK
                            );
                        }
                    }
                    storage.apply_to_world(world, &snap);
                }

                let neighbor_idx = (w_idx + 1) % ACTIVE_WORLDS;
                let probe_dense = Arc::new(ParallelWorldProbe {
                    expected_sym_value: expected_sym,
                    foreign_sym_cstr: std::ffi::CString::new(format!(
                        "bam.v0.WorldPrivateSym_{neighbor_idx}"
                    ))
                    .unwrap(),
                    is_sparse_rows: false,
                    failed: AtomicBool::new(false),
                    runs: AtomicUsize::new(0),
                });
                let probe_sparse = Arc::new(ParallelWorldProbe {
                    expected_sym_value: expected_sym,
                    foreign_sym_cstr: std::ffi::CString::new(format!(
                        "bam.v0.WorldPrivateSym_{neighbor_idx}"
                    ))
                    .unwrap(),
                    is_sparse_rows: true,
                    failed: AtomicBool::new(false),
                    runs: AtomicUsize::new(0),
                });

                let reg = app.world().resource::<ModRegistry>().clone();

                let sys_dense_with = GuestSystem::new(
                    format!("w{w_idx}_dense_with_badge"),
                    vec![BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![
                            BamTermDesc::write(schema.dense_vec),
                            BamTermDesc::filter_only(schema.sparse_badge),
                        ],
                    }],
                    verify_worker_isolation_and_mutate,
                    Arc::as_ptr(&probe_dense) as *mut c_void,
                    reg.clone(),
                );
                let sys_dense_without = GuestSystem::new(
                    format!("w{w_idx}_dense_without_badge"),
                    vec![BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![
                            BamTermDesc::write(schema.dense_vec),
                            BamTermDesc::without(schema.sparse_badge),
                        ],
                    }],
                    verify_worker_isolation_and_mutate,
                    Arc::as_ptr(&probe_dense) as *mut c_void,
                    reg.clone(),
                );
                let sys_sparse = GuestSystem::new(
                    format!("w{w_idx}_sparse_energy"),
                    vec![BamQuerySpecOwned {
                        flags: 0,
                        terms: vec![BamTermDesc::write(schema.sparse_energy)],
                    }],
                    verify_worker_isolation_and_mutate,
                    Arc::as_ptr(&probe_sparse) as *mut c_void,
                    reg,
                );

                app.add_systems(Update, (sys_dense_with, sys_dense_without, sys_sparse));

                for frame in 0..5 {
                    app.update();
                    if frame == 2 {
                        for &e in &spawned[..4] {
                            app.world_mut().despawn(e);
                        }
                        assert_eq!(PARALLEL_WORLD_DROPS[w_idx].load(Ordering::SeqCst), 4);
                    }
                }

                assert!(
                    !probe_dense.failed.load(Ordering::SeqCst)
                        && !probe_sparse.failed.load(Ordering::SeqCst)
                );
                assert_eq!(
                    probe_dense.runs.load(Ordering::SeqCst)
                        + probe_sparse.runs.load(Ordering::SeqCst),
                    15
                );

                for &e in &spawned[4..] {
                    assert_eq!(app.world().get::<SparseEnergy>(e).unwrap().energy, 15);
                    app.world_mut().despawn(e);
                }

                drop(app);
                drop(probe_dense);
                drop(probe_sparse);
            }));
        }

        for h in handles {
            h.join().expect("parallel world thread panicked");
        }
    });

    for slot in PARALLEL_WORLD_DROPS.iter().take(ACTIVE_WORLDS) {
        assert_eq!(slot.load(Ordering::SeqCst), ENTITIES_PER_WORLD);
    }
    drop(poison_reg);
}

static WORLD_A_DROPS: AtomicUsize = AtomicUsize::new(0);
static WORLD_B_DROPS: AtomicUsize = AtomicUsize::new(0);

unsafe extern "C" fn drop_world_a(_ptr: *mut c_void) {
    WORLD_A_DROPS.fetch_add(1, Ordering::SeqCst);
}

unsafe extern "C" fn drop_world_b(_ptr: *mut c_void) {
    WORLD_B_DROPS.fetch_add(1, Ordering::SeqCst);
}

#[test]
fn two_worlds_same_uuid_keep_isolated_drop_callbacks_including_world_drop() {
    WORLD_A_DROPS.store(0, Ordering::SeqCst);
    WORLD_B_DROPS.store(0, Ordering::SeqCst);

    let shared_uuid = ComponentUuid::from_parts("diff_parity::SharedCrossWorldComp", 8, 8, 55);

    let mut world_a = World::new();
    world_a.insert_resource(WorldDropGuard::new());
    let reg_a = ModRegistry::new();
    world_a.insert_resource(reg_a.clone());
    let active_a = Arc::new(AtomicBool::new(true));
    let handle_a = {
        let mut ctx_a = LoadContext::with_world_and_mod_id(
            reg_a,
            Arc::clone(&active_a),
            &mut world_a as *mut World,
            None,
        );
        let desc_a = ComponentDesc::new(
            c"diff_parity::SharedCrossWorldComp".as_ptr(),
            shared_uuid,
            8,
            8,
        )
        .with_drop(drop_world_a);
        let mut h = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(ctx_a.as_ffi_ptr(), &desc_a, &mut h) },
            FfiResult::OK
        );
        ctx_a.finish_init();
        h
    };

    let mut world_b = World::new();
    world_b.insert_resource(WorldDropGuard::new());
    let reg_b = ModRegistry::new();
    world_b.insert_resource(reg_b.clone());
    let active_b = Arc::new(AtomicBool::new(true));
    let handle_b = {
        let mut ctx_b = LoadContext::with_world_and_mod_id(
            reg_b,
            Arc::clone(&active_b),
            &mut world_b as *mut World,
            None,
        );
        let desc_b = ComponentDesc::new(
            c"diff_parity::SharedCrossWorldComp".as_ptr(),
            shared_uuid,
            8,
            8,
        )
        .with_drop(drop_world_b);
        let mut h = BamComponent::INVALID;
        assert_eq!(
            unsafe { host_register_component(ctx_b.as_ffi_ptr(), &desc_b, &mut h) },
            FfiResult::OK
        );
        ctx_b.finish_init();
        h
    };

    for (world, handle) in [(&mut world_a, handle_a), (&mut world_b, handle_b)] {
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
            let rq = cmds.as_ffi_ptr();
            let mut e = FfiEntity::NULL;
            assert_eq!(
                unsafe { host_cmd_spawn(rq, core::ptr::null(), core::ptr::null(), 0, &mut e) },
                FfiResult::OK
            );
            let bytes = [1u8; 8];
            assert_eq!(
                unsafe { host_cmd_insert(rq, e, handle, bytes.as_ptr(), 8) },
                FfiResult::OK
            );
        }
        storage.apply_to_world(world, &snap);
    }

    world_a.clear_entities();
    assert_eq!(WORLD_A_DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(WORLD_B_DROPS.load(Ordering::SeqCst), 0);
    drop(world_a);

    world_b.clear_entities();
    assert_eq!(WORLD_A_DROPS.load(Ordering::SeqCst), 1);
    assert_eq!(WORLD_B_DROPS.load(Ordering::SeqCst), 1);
    drop(world_b);
}

unsafe extern "C" fn probe_proc_address_in_executor(
    sys_ctx: *mut BamSystemCtx,
    user_data: *mut c_void,
) -> FfiResult {
    let host_ctx = unsafe { host_sys_host_ctx(sys_ctx) };
    assert!(!host_ctx.is_null());
    let foreign_sym =
        unsafe { host_get_proc_address(host_ctx, c"bam.v0.OnlyInOtherWorldSymbol".as_ptr()) };
    let own_sym = unsafe { host_get_proc_address(host_ctx, c"bam.v0.OwnWorldSymbol".as_ptr()) };
    let passed = foreign_sym.is_null() && own_sym == core::ptr::without_provenance(0x1234);
    unsafe { (*(user_data as *const AtomicBool)).store(passed, Ordering::SeqCst) };
    FfiResult::OK
}

#[test]
fn runtime_get_proc_address_on_executor_is_isolated_to_current_world_registry() {
    let mut other_reg = ModRegistry::new();
    unsafe {
        other_reg.register_custom_symbol(
            "bam.v0.OnlyInOtherWorldSymbol",
            core::ptr::without_provenance(0x9999),
        );
    }

    let mut app = App::new();
    app.add_plugins(AbiModPlugin);
    unsafe {
        app.world_mut()
            .resource_mut::<ModRegistry>()
            .register_custom_symbol(
                "bam.v0.OwnWorldSymbol",
                core::ptr::without_provenance(0x1234),
            );
    }

    let ok_flag = Arc::new(AtomicBool::new(false));
    let reg = app.world().resource::<ModRegistry>().clone();
    let sys = GuestSystem::new(
        "probe_symbols",
        vec![],
        probe_proc_address_in_executor,
        Arc::as_ptr(&ok_flag) as *mut c_void,
        reg,
    );
    app.add_systems(Update, sys);
    app.update();

    assert!(ok_flag.load(Ordering::SeqCst));
    drop(other_reg);
}

// ============================================================================
// Comprehensive `proptest!` Differential Parity Suite
// ============================================================================

mod proptest_differential_parity {
    use super::*;

    fn proptest_cases() -> u32 {
        if let Ok(raw) = std::env::var("PROPTEST_CASES")
            && let Ok(parsed) = raw.parse::<u32>()
            && parsed > 0
        {
            return parsed;
        }
        #[cfg(miri)]
        {
            32
        }
        #[cfg(not(miri))]
        {
            512
        }
    }

    fn mode_from_tag(tag: u8, is_resource: bool) -> AccessMode {
        if is_resource {
            if (tag & 1) == 0 {
                AccessMode::RESOURCE_READ
            } else {
                AccessMode::RESOURCE_WRITE
            }
        } else {
            match tag % 8 {
                0 => AccessMode::READ,
                1 => AccessMode::WRITE,
                2 => AccessMode::FILTER_ONLY,
                3 => AccessMode::WITHOUT,
                4 => AccessMode::OPTIONAL_READ,
                5 => AccessMode::OPTIONAL_WRITE,
                6 => AccessMode::CHANGED,
                _ => AccessMode::ADDED,
            }
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum WriteStyle {
        /// Blanket chunk-level `iter_column_mutable` / `iter_column_rows_mutable`.
        BlanketMutable,
        /// Selective row-level `iter_column_mutable_untracked` + `iter_mark_changed_range`.
        SelectiveOddRows,
    }

    #[derive(Clone, Debug)]
    struct ValidTermChoice {
        comp_idx: u8,
        mode_tag: u8,
    }

    #[derive(Clone, Debug)]
    struct MutationStep {
        entity_slot: u8,
        comp_mask: u8,
        spawn_extra: bool,
    }

    fn build_valid_query_spec(
        schema: &SchemaHandles,
        choices: &[ValidTermChoice],
    ) -> BamQuerySpecOwned {
        let entity_handles = schema.entity_handles();
        let resource_handles = schema.resource_handles();

        let mut seen_read = BTreeSet::new();
        let mut seen_write = BTreeSet::new();
        let mut seen_res = BTreeSet::new();
        let mut terms = Vec::with_capacity(choices.len());

        for ch in choices {
            let idx = (ch.comp_idx as usize) % 9;
            if idx >= 7 {
                let r_idx = idx - 7;
                if seen_res.insert(r_idx) {
                    let mode = mode_from_tag(ch.mode_tag, true);
                    terms.push(BamTermDesc::new(resource_handles[r_idx], mode));
                }
                continue;
            }

            let h = entity_handles[idx];
            let mut mode = mode_from_tag(ch.mode_tag, false);

            // Deduplicate / normalize conflicting write modes on the same component so generated
            // queries stay valid (while still allowing `WRITE + CHANGED`, `READ + READ`, `FILTER_ONLY`, etc.).
            match mode {
                AccessMode::WRITE | AccessMode::OPTIONAL_WRITE => {
                    if seen_read.contains(&idx) || seen_write.contains(&idx) {
                        mode = AccessMode::FILTER_ONLY;
                    } else {
                        seen_write.insert(idx);
                    }
                }
                AccessMode::READ | AccessMode::OPTIONAL_READ => {
                    if seen_write.contains(&idx) {
                        mode = AccessMode::CHANGED;
                    } else {
                        seen_read.insert(idx);
                    }
                }
                _ => {}
            }

            terms.push(BamTermDesc::new(h, mode));
        }

        BamQuerySpecOwned { flags: 0, terms }
    }

    fn spawn_entity_from_mask(world: &mut World, mask: u8, seed_val: i64) -> Entity {
        let mut emut = world.spawn_empty();
        if (mask & (1 << 0)) != 0 {
            emut.insert(DenseVec {
                x: seed_val,
                y: -seed_val,
            });
        }
        if (mask & (1 << 1)) != 0 {
            emut.insert(OverAligned32 {
                lanes: [seed_val, seed_val + 1, seed_val + 2, seed_val + 3],
            });
        }
        if (mask & (1 << 2)) != 0 {
            emut.insert(DenseZst);
        }
        if (mask & (1 << 3)) != 0 {
            emut.insert(Aligned16Zst);
        }
        if (mask & (1 << 4)) != 0 {
            emut.insert(SparseEnergy {
                energy: 100 + seed_val,
            });
        }
        if (mask & (1 << 5)) != 0 {
            emut.insert(SparseBadge {
                tier: (seed_val as u32) & 7,
            });
        }
        if (mask & (1 << 6)) != 0 {
            emut.insert(SparseZst);
        }
        emut.id()
    }

    fn apply_mutation_step(world: &mut World, entities: &mut Vec<Entity>, step: &MutationStep) {
        world.increment_change_tick();
        if !entities.is_empty() {
            let e = entities[(step.entity_slot as usize) % entities.len()];
            let mut emut = world.entity_mut(e);
            if (step.comp_mask & (1 << 0)) != 0
                && let Some(mut dv) = emut.get_mut::<DenseVec>()
            {
                dv.x = dv.x.wrapping_add(1);
            }
            if (step.comp_mask & (1 << 1)) != 0
                && let Some(mut oa) = emut.get_mut::<OverAligned32>()
            {
                oa.lanes[0] = oa.lanes[0].wrapping_add(1);
            }
            if (step.comp_mask & (1 << 2)) != 0
                && let Some(mut dz) = emut.get_mut::<DenseZst>()
            {
                dz.set_changed();
            }
            if (step.comp_mask & (1 << 3)) != 0
                && let Some(mut az) = emut.get_mut::<Aligned16Zst>()
            {
                az.set_changed();
            }
            if (step.comp_mask & (1 << 4)) != 0
                && let Some(mut se) = emut.get_mut::<SparseEnergy>()
            {
                se.energy = se.energy.wrapping_add(5);
            }
            if (step.comp_mask & (1 << 5)) != 0
                && let Some(mut sb) = emut.get_mut::<SparseBadge>()
            {
                sb.tier = sb.tier.wrapping_add(1);
            }
            if (step.comp_mask & (1 << 6)) != 0
                && let Some(mut sz) = emut.get_mut::<SparseZst>()
            {
                sz.set_changed();
            }
        }

        if step.spawn_extra {
            let new_e = spawn_entity_from_mask(world, step.comp_mask | 1, 99);
            entities.push(new_e);
        }
    }

    struct DynamicProbeContext {
        spec: BamQuerySpecOwned,
        registry: ModRegistry,
        style: WriteStyle,
        visited_entities: Mutex<Vec<u64>>,
        res_reads_ok: AtomicBool,
    }

    unsafe extern "C" fn dynamic_parity_probe_cb(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        let ctx = unsafe { &*(user_data as *const DynamicProbeContext) };
        let mut iter: *mut BamIter = core::ptr::null_mut();
        assert_eq!(
            unsafe { host_sys_query(sys_ctx, 0, &mut iter) },
            FfiResult::OK
        );

        let snap = ctx.registry.snapshot();

        // Exercise `res_ptr` and `res_mut_ptr` on any singleton resource terms.
        for (t_idx, term) in ctx.spec.terms.iter().enumerate() {
            if term.mode == AccessMode::RESOURCE_READ {
                let p = unsafe { host_res_ptr(iter, t_idx) };
                if p.is_null() {
                    ctx.res_reads_ok.store(false, Ordering::SeqCst);
                    return FfiResult::NULL_POINTER;
                }
            } else if term.mode == AccessMode::RESOURCE_WRITE {
                let p = unsafe { host_res_mut_ptr(iter, t_idx) };
                if p.is_null() {
                    ctx.res_reads_ok.store(false, Ordering::SeqCst);
                    return FfiResult::NULL_POINTER;
                }
                let meta = snap.get_by_handle(term.component).unwrap();
                if meta.name.as_ref() == "diff_parity::GlobalBiasRes" {
                    unsafe {
                        (*(p as *mut GlobalBiasRes)).writes += 1;
                    }
                } else {
                    unsafe {
                        (*(p as *mut GlobalFlagRes)).mask ^= 1;
                    }
                }
            }
        }

        let mut visited = ctx.visited_entities.lock().unwrap();
        let mut count = 0usize;
        let mut global_row_idx = 0usize;

        while unsafe { host_iter_next_chunk(iter, &mut count) }.get() {
            let ents = unsafe { host_iter_entities(iter) };
            assert!(!ents.is_null());
            for r in 0..count {
                let e_bits = unsafe { (*ents.add(r)).0 };
                visited.push(e_bits);
            }

            // Exercise column pointers and change-tick marking for every entity term.
            for (t_idx, term) in ctx.spec.terms.iter().enumerate() {
                let meta = snap.get_by_handle(term.component).unwrap();
                if meta.is_resource() || !term.mode.has_entity_column_data() {
                    continue;
                }

                // Skip read-side pointer check if same component also appears as WRITE/OPTIONAL_WRITE
                // in this query (suppressed by `QueryPlan` to prevent aliasing).
                if term.mode.is_tick_filter()
                    && ctx.spec.terms.iter().any(|o| {
                        o.component == term.component
                            && (o.mode == AccessMode::WRITE || o.mode == AccessMode::OPTIONAL_WRITE)
                    })
                {
                    continue;
                }

                let is_mutable =
                    term.mode == AccessMode::WRITE || term.mode == AccessMode::OPTIONAL_WRITE;

                if meta.is_sparse_set() {
                    if is_mutable {
                        match ctx.style {
                            WriteStyle::BlanketMutable => {
                                let rows = unsafe { host_iter_column_rows_mutable(iter, t_idx) };
                                if term.mode == AccessMode::WRITE {
                                    assert!(!rows.is_null());
                                }
                            }
                            WriteStyle::SelectiveOddRows => {
                                let rows = unsafe { host_iter_column_rows_readonly(iter, t_idx) };
                                if !rows.is_null() {
                                    for r in 0..count {
                                        if (global_row_idx + r) % 2 == 1 {
                                            assert_eq!(
                                                unsafe {
                                                    host_iter_mark_changed_range(iter, t_idx, r, 1)
                                                },
                                                FfiResult::OK
                                            );
                                        }
                                    }
                                } else if term.mode == AccessMode::WRITE {
                                    return FfiResult::NULL_POINTER;
                                }
                            }
                        }
                    } else {
                        let rows = unsafe { host_iter_column_rows_readonly(iter, t_idx) };
                        if term.mode != AccessMode::OPTIONAL_READ {
                            assert!(!rows.is_null());
                        }
                    }
                } else if is_mutable {
                    match ctx.style {
                        WriteStyle::BlanketMutable => {
                            let col = unsafe { host_iter_column_mutable(iter, t_idx) };
                            if term.mode == AccessMode::WRITE {
                                assert!(!col.is_null());
                                if meta.align > 1 {
                                    assert_eq!((col as usize) % meta.align, 0);
                                }
                            }
                        }
                        WriteStyle::SelectiveOddRows => {
                            let col = unsafe { host_iter_column_mutable_untracked(iter, t_idx) };
                            if !col.is_null() {
                                if meta.align > 1 {
                                    assert_eq!((col as usize) % meta.align, 0);
                                }
                                for r in 0..count {
                                    if (global_row_idx + r) % 2 == 1 {
                                        assert_eq!(
                                            unsafe {
                                                host_iter_mark_changed_range(iter, t_idx, r, 1)
                                            },
                                            FfiResult::OK
                                        );
                                    }
                                }
                            } else if term.mode == AccessMode::WRITE {
                                return FfiResult::NULL_POINTER;
                            }
                        }
                    }
                } else {
                    let col = unsafe { host_iter_column_readonly(iter, t_idx) };
                    if term.mode != AccessMode::OPTIONAL_READ {
                        assert!(!col.is_null());
                        if meta.align > 1 {
                            assert_eq!((col as usize) % meta.align, 0);
                        }
                    }
                }
            }

            global_row_idx += count;
        }

        FfiResult::OK
    }

    /// Reference oracle executing native Bevy `QueryBuilder::<Entity>` + exact `Tick::is_newer_than`
    /// filtering and applying identical change-tick mutations on `world_nat`.
    fn run_native_reference_step(
        world: &mut World,
        registry: &ModRegistry,
        spec: &BamQuerySpecOwned,
        last_run: Tick,
        style: WriteStyle,
    ) -> (Vec<u64>, Tick) {
        let this_run = world.increment_change_tick();
        let snap = registry.snapshot();

        // Apply resource mutations identically.
        for term in &spec.terms {
            if term.mode == AccessMode::RESOURCE_WRITE {
                let meta = snap.get_by_handle(term.component).unwrap();
                if meta.name.as_ref() == "diff_parity::GlobalBiasRes" {
                    world.resource_mut::<GlobalBiasRes>().writes += 1;
                } else {
                    world.resource_mut::<GlobalFlagRes>().mask ^= 1;
                }
            }
        }

        let has_entity_terms =
            spec.terms.is_empty() || spec.terms.iter().any(|t| !t.mode.is_resource());
        if !has_entity_terms {
            return (Vec::new(), this_run);
        }

        let mut seen_write = BTreeSet::new();
        for t in &spec.terms {
            if t.mode == AccessMode::WRITE || t.mode == AccessMode::OPTIONAL_WRITE {
                seen_write.insert(snap.get_by_handle(t.component).unwrap().component_id);
            }
        }

        let mut builder = QueryBuilder::<()>::new(world);
        let mut tick_filters: Vec<(ComponentId, bool)> = Vec::new();
        let mut write_terms: Vec<(ComponentId, AccessMode)> = Vec::new();
        let mut sparse_data_mask = 0u32;

        for (t_idx, t) in spec.terms.iter().enumerate() {
            let meta = snap.get_by_handle(t.component).unwrap();
            if meta.is_resource() {
                continue;
            }
            let cid = meta.component_id;
            if t.mode.has_entity_column_data()
                && !(t.mode.is_tick_filter() && seen_write.contains(&cid))
                && meta.is_sparse_set()
            {
                sparse_data_mask |= 1u32 << t_idx;
            }
            match t.mode {
                AccessMode::READ => {
                    builder.ref_id(cid);
                }
                AccessMode::WRITE => {
                    builder.mut_id(cid);
                    write_terms.push((cid, t.mode));
                }
                AccessMode::OPTIONAL_READ => {
                    builder.optional(|b| {
                        b.ref_id(cid);
                    });
                }
                AccessMode::OPTIONAL_WRITE => {
                    builder.optional(|b| {
                        b.mut_id(cid);
                    });
                    write_terms.push((cid, t.mode));
                }
                AccessMode::FILTER_ONLY => {
                    builder.with_id(cid);
                }
                AccessMode::WITHOUT => {
                    builder.without_id(cid);
                }
                AccessMode::CHANGED => {
                    if seen_write.contains(&cid) {
                        builder.with_id(cid);
                    } else {
                        builder.ref_id(cid);
                    }
                    tick_filters.push((cid, false));
                }
                AccessMode::ADDED => {
                    if seen_write.contains(&cid) {
                        builder.with_id(cid);
                    } else {
                        builder.ref_id(cid);
                    }
                    tick_filters.push((cid, true));
                }
                _ => {}
            }
        }

        let mut query_state = builder.build();
        query_state.update_archetypes(world);
        let oracle_set_from_query_builder: BTreeSet<u64> = {
            let mut qb = QueryBuilder::<Entity>::new(world);
            for t in &spec.terms {
                let meta = snap.get_by_handle(t.component).unwrap();
                if meta.is_resource() {
                    continue;
                }
                let cid = meta.component_id;
                match t.mode {
                    AccessMode::READ => {
                        qb.ref_id(cid);
                    }
                    AccessMode::WRITE => {
                        qb.mut_id(cid);
                    }
                    AccessMode::OPTIONAL_READ => {
                        qb.optional(|b| {
                            b.ref_id(cid);
                        });
                    }
                    AccessMode::OPTIONAL_WRITE => {
                        qb.optional(|b| {
                            b.mut_id(cid);
                        });
                    }
                    AccessMode::FILTER_ONLY => {
                        qb.with_id(cid);
                    }
                    AccessMode::WITHOUT => {
                        qb.without_id(cid);
                    }
                    AccessMode::CHANGED | AccessMode::ADDED => {
                        if seen_write.contains(&cid) {
                            qb.with_id(cid);
                        } else {
                            qb.ref_id(cid);
                        }
                    }
                    _ => {}
                }
            }
            let mut qs = qb.build();
            qs.iter(world).map(Entity::to_bits).collect()
        };
        let is_comp_dense = |id| {
            world
                .components()
                .get_info(id)
                .is_some_and(|info| info.storage_type() == bevy_ecs::component::StorageType::Table)
        };
        let is_dense = sparse_data_mask == 0
            && query_state
                .component_access()
                .with_filters()
                .all(is_comp_dense)
            && query_state
                .component_access()
                .without_filters()
                .all(is_comp_dense);

        let mut candidate_entities: Vec<Entity> = Vec::new();
        if is_dense {
            let matched_tables: Vec<_> = query_state.matched_tables().collect();
            let tables = &world.storages().tables;
            for tid in matched_tables {
                if let Some(table) = tables.get(tid) {
                    candidate_entities.extend_from_slice(table.entities());
                }
            }
        } else {
            let matched_archs: Vec<_> = query_state.matched_archetypes().collect();
            let archetypes = world.archetypes();
            for aid in matched_archs {
                if let Some(arch) = archetypes.get(aid) {
                    candidate_entities.extend(arch.entities().iter().map(|ae| ae.id()));
                }
            }
        }
        assert_eq!(
            candidate_entities
                .iter()
                .copied()
                .map(Entity::to_bits)
                .collect::<BTreeSet<_>>(),
            oracle_set_from_query_builder
        );

        // Filter candidates by `Changed<T>` / `Added<T>` against `(last_run, this_run)`.
        let mut matched: Vec<Entity> = Vec::with_capacity(candidate_entities.len());
        for e in candidate_entities {
            let er = world.entity(e);
            let mut ok = true;
            for &(cid, is_added) in &tick_filters {
                let Some(ticks) = er.get_change_ticks_by_id(cid) else {
                    ok = false;
                    break;
                };
                let tick = if is_added { ticks.added } else { ticks.changed };
                if !tick.is_newer_than(last_run, this_run) {
                    ok = false;
                    break;
                }
            }
            if ok {
                matched.push(e);
            }
        }

        // Apply identical `this_run` tick updates on writable columns for `matched` rows!
        for (row_idx, &e) in matched.iter().enumerate() {
            let should_mark = match style {
                WriteStyle::BlanketMutable => true,
                WriteStyle::SelectiveOddRows => row_idx % 2 == 1,
            };
            if !should_mark {
                continue;
            }
            for &(cid, _mode) in &write_terms {
                let cell = world.as_unsafe_world_cell();
                if let Ok(ec) = cell.get_entity_with_ticks(e, last_run, this_run) {
                    // SAFETY: Exclusive `&mut World` access; `get_mut_by_id` marks `changed` tick + summary tick to `this_run`.
                    let _ = unsafe { ec.get_mut_by_id(cid) };
                }
            }
        }

        let visited_bits = matched.into_iter().map(Entity::to_bits).collect();
        (visited_bits, this_run)
    }

    fn collect_changed_sets(
        world: &mut World,
        since_tick: Tick,
        this_run: Tick,
    ) -> BTreeMap<usize, BTreeSet<u64>> {
        let snap = world.resource::<ModRegistry>().snapshot_arc();
        let mut out = BTreeMap::new();
        let mut q = world.query::<Entity>();
        let all_entities: Vec<Entity> = q.iter(world).collect();

        for (idx, entry) in snap.entries().iter().take(7).enumerate() {
            let mut set = BTreeSet::new();
            for &e in &all_entities {
                if let Some(ticks) = world.entity(e).get_change_ticks_by_id(entry.component_id)
                    && ticks.changed.is_newer_than(since_tick, this_run)
                {
                    set.insert(e.to_bits());
                }
            }
            out.insert(idx, set);
        }
        out
    }

    fn arb_term_choice() -> impl Strategy<Value = ValidTermChoice> {
        (0u8..9u8, 0u8..16u8)
            .prop_map(|(comp_idx, mode_tag)| ValidTermChoice { comp_idx, mode_tag })
    }

    fn arb_query_group() -> impl Strategy<Value = Vec<ValidTermChoice>> {
        prop::collection::vec(arb_term_choice(), 0..=6)
    }

    fn arb_mutation_step() -> impl Strategy<Value = MutationStep> {
        (0u8..32u8, any::<u8>(), any::<bool>()).prop_map(|(entity_slot, comp_mask, spawn_extra)| {
            MutationStep {
                entity_slot,
                comp_mask,
                spawn_extra,
            }
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: proptest_cases(),
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        /// Differential `FilteredAccessSet` self-conflict and pairwise compatibility
        /// parity across generated multi-query term sets (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`,
        /// `OPTIONAL_READ`, `OPTIONAL_WRITE`, `CHANGED`, `ADDED`, `RESOURCE_READ`, `RESOURCE_WRITE`)
        /// over dense `Table` (including ZST + 32B over-aligned) and `SparseSet` components.
        #[test]
        fn access_compatibility_and_self_conflict_parity(
            raw_groups_a in prop::collection::vec(arb_query_group(), 1..=3),
            raw_groups_b in prop::collection::vec(arb_query_group(), 1..=3),
        ) {
            let mut app = App::new();
            app.add_plugins(AbiModPlugin);
            let schema = register_full_schema(&mut app);
            let entity_handles = schema.entity_handles();
            let resource_handles = schema.resource_handles();
            let reg = app.world().resource::<ModRegistry>().clone();

            let to_specs = |raw_groups: &[Vec<ValidTermChoice>]| -> Vec<BamQuerySpecOwned> {
                raw_groups
                    .iter()
                    .map(|g| {
                        let terms = g
                            .iter()
                            .map(|ch| {
                                let idx = (ch.comp_idx as usize) % 9;
                                if idx >= 7 {
                                    BamTermDesc::new(
                                        resource_handles[idx - 7],
                                        mode_from_tag(ch.mode_tag, true),
                                    )
                                } else {
                                    BamTermDesc::new(
                                        entity_handles[idx],
                                        mode_from_tag(ch.mode_tag, false),
                                    )
                                }
                            })
                            .collect();
                        BamQuerySpecOwned { flags: 0, terms }
                    })
                    .collect()
            };

            let specs_a = to_specs(&raw_groups_a);
            let specs_b = to_specs(&raw_groups_b);

            let mut sys_a = GuestSystem::new("prop_sys_a", specs_a.clone(), noop_cb, core::ptr::null_mut(), reg.clone());
            let mut sys_b = GuestSystem::new("prop_sys_b", specs_b.clone(), noop_cb, core::ptr::null_mut(), reg.clone());

            let acc_a = sys_a.initialize(app.world_mut());
            let acc_b = sys_b.initialize(app.world_mut());

            let (nat_conflict_a, nat_set_a) = build_native_reference_access(app.world_mut(), &reg, &specs_a);
            let (nat_conflict_b, nat_set_b) = build_native_reference_access(app.world_mut(), &reg, &specs_b);

            prop_assert_eq!(sys_a.has_self_conflict(), nat_conflict_a);
            prop_assert_eq!(sys_b.has_self_conflict(), nat_conflict_b);

            if let (SystemAccess::Shared(dyn_a), SystemAccess::Shared(dyn_b)) = (acc_a, acc_b) {
                prop_assert!(!nat_conflict_a && !nat_conflict_b);
                prop_assert_eq!(
                    dyn_a.is_compatible(&dyn_b),
                    nat_set_a.is_compatible(&nat_set_b)
                );
            }
        }

        /// Differential iteration & change-tick parity comparing `GuestSystem` +
        /// `HostIter` (dense tick skip, indirect sparse batching, blanket vs
        /// selective `iter_mark_changed_range`) against native Bevy `QueryBuilder` across random
        /// multi-archetype entity populations and multi-step mutation histories.
        #[test]
        fn iteration_order_and_change_tick_parity(
            archetype_masks in prop::collection::vec(any::<u8>(), 1..=16),
            choices in prop::collection::vec(arb_term_choice(), 1..=6),
            mutations_frame1 in prop::collection::vec(arb_mutation_step(), 0..=4),
            mutations_frame2 in prop::collection::vec(arb_mutation_step(), 0..=4),
            selective_writes in any::<bool>(),
        ) {
            let mut app_nat = App::new();
            app_nat.add_plugins(AbiModPlugin);
            let schema_nat = register_full_schema(&mut app_nat);

            let mut app_dyn = App::new();
            app_dyn.add_plugins(AbiModPlugin);
            let schema_dyn = register_full_schema(&mut app_dyn);

            let mut ents_nat = Vec::with_capacity(archetype_masks.len());
            let mut ents_dyn = Vec::with_capacity(archetype_masks.len());
            for (i, &mask) in archetype_masks.iter().enumerate() {
                ents_nat.push(spawn_entity_from_mask(app_nat.world_mut(), mask, (i + 1) as i64));
                ents_dyn.push(spawn_entity_from_mask(app_dyn.world_mut(), mask, (i + 1) as i64));
            }

            let spec_nat = build_valid_query_spec(&schema_nat, &choices);
            let spec_dyn = build_valid_query_spec(&schema_dyn, &choices);

            let style = if selective_writes {
                WriteStyle::SelectiveOddRows
            } else {
                WriteStyle::BlanketMutable
            };

            let reg_nat = app_nat.world().resource::<ModRegistry>().clone();
            let reg_dyn = app_dyn.world().resource::<ModRegistry>().clone();

            let probe_ctx = Arc::new(DynamicProbeContext {
                spec: spec_dyn.clone(),
                registry: reg_dyn.clone(),
                style,
                visited_entities: Mutex::new(Vec::new()),
                res_reads_ok: AtomicBool::new(true),
            });

            let mut dyn_sys = GuestSystem::new(
                "proptest_parity_sys",
                vec![spec_dyn],
                dynamic_parity_probe_cb,
                Arc::as_ptr(&probe_ctx) as *mut c_void,
                reg_dyn,
            );
            let init_access = dyn_sys.initialize(app_dyn.world_mut());
            prop_assert!(matches!(init_access, SystemAccess::Shared(_)));

            // Sync initial `last_run` between native oracle and `dyn_sys`.
            let mut nat_last_run = dyn_sys.get_last_run();

            for mutations in [&mutations_frame1, &mutations_frame2] {
                for step in mutations {
                    apply_mutation_step(app_nat.world_mut(), &mut ents_nat, step);
                    apply_mutation_step(app_dyn.world_mut(), &mut ents_dyn, step);
                }

                let pre_run_tick_nat = app_nat.world().read_change_tick();
                let pre_run_tick_dyn = app_dyn.world().read_change_tick();
                prop_assert_eq!(pre_run_tick_nat, pre_run_tick_dyn);

                probe_ctx.visited_entities.lock().unwrap().clear();
                dyn_sys.run((), app_dyn.world_mut()).unwrap();
                prop_assert!(probe_ctx.res_reads_ok.load(Ordering::SeqCst));
                let dyn_visited = probe_ctx.visited_entities.lock().unwrap().clone();

                let (nat_visited, new_nat_last_run) = run_native_reference_step(
                    app_nat.world_mut(),
                    &reg_nat,
                    &spec_nat,
                    nat_last_run,
                    style,
                );
                nat_last_run = new_nat_last_run;

                // 1. Exact entity set & exact per-table/archetype row iteration order parity!
                prop_assert_eq!(&dyn_visited, &nat_visited);

                // 2. Exact post-run `Changed<T>` tick effects on every schema component!
                let post_tick_nat = app_nat.world().read_change_tick();
                let post_tick_dyn = app_dyn.world().read_change_tick();
                let changed_nat = collect_changed_sets(app_nat.world_mut(), pre_run_tick_nat, post_tick_nat);
                let changed_dyn = collect_changed_sets(app_dyn.world_mut(), pre_run_tick_dyn, post_tick_dyn);
                prop_assert_eq!(changed_dyn, changed_nat);

                // 3. Exact resource state parity!
                prop_assert_eq!(
                    *app_dyn.world().resource::<GlobalBiasRes>(),
                    *app_nat.world().resource::<GlobalBiasRes>()
                );
                prop_assert_eq!(
                    *app_dyn.world().resource::<GlobalFlagRes>(),
                    *app_nat.world().resource::<GlobalFlagRes>()
                );
            }
        }
    }
}
