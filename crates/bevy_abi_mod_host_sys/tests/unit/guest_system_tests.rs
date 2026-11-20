use super::*;
use crate::ffi::iter::host_iter_next_chunk;
use crate::registry::ComponentOptions;
use crate::system::error_policy::{BudgetWatchdogAction, ModBudgetHook};
use bevy_abi_mod_sys::{
    AbiPod, BAM_STORAGE_FLAG_SUMMARY_TICK, BamTermDesc, ComponentUuid, ModComponent, RawInsertable,
    SymbolEntry, VTableHeader,
};
use bevy_ecs::component::Component;
use core::ffi::CStr;
use core::sync::atomic::AtomicUsize;
use std::sync::Mutex;

#[repr(C)]
#[derive(Component, Clone, Copy, Default)]
#[component(summary_tick)]
struct SummaryVal(f32);

// SAFETY: `f32` has universal bit validity.
unsafe impl RawInsertable for SummaryVal {}
// SAFETY: `#[repr(C)]` `f32` POD.
unsafe impl AbiPod for SummaryVal {}
impl ModComponent for SummaryVal {
    const CANONICAL_NAME: &'static str = "guest_sys_unit::SummaryVal";
    const COMPONENT_CSTR: &'static CStr = c"guest_sys_unit::SummaryVal";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("guest_sys_unit::SummaryVal");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;
    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader = VTableHeader::new::<VTableHeader, SummaryVal>(SummaryVal::UUID);
        &VT
    }
    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

#[repr(C)]
#[derive(Component, Clone, Copy, Default)]
#[component(storage = "SparseSet")]
struct SparseFlag(u32);

// SAFETY: `u32` has universal bit validity.
unsafe impl RawInsertable for SparseFlag {}
// SAFETY: `#[repr(C)]` `u32` POD.
unsafe impl AbiPod for SparseFlag {}
impl ModComponent for SparseFlag {
    const CANONICAL_NAME: &'static str = "guest_sys_unit::SparseFlag";
    const COMPONENT_CSTR: &'static CStr = c"guest_sys_unit::SparseFlag";
    const UUID: ComponentUuid = ComponentUuid::from_canonical_name("guest_sys_unit::SparseFlag");
    const IS_ABI_POD: bool = true;
    const IS_RAW_INSERTABLE: bool = true;
    type VTable = VTableHeader;
    fn vtable() -> &'static Self::VTable {
        static VT: VTableHeader = VTableHeader::new::<VTableHeader, SparseFlag>(SparseFlag::UUID);
        &VT
    }
    fn symbols() -> &'static [SymbolEntry] {
        &[]
    }
}

struct RecordingBudgetHook {
    quarantine_on_run: AtomicUsize,
    run_count: AtomicUsize,
    runs: Mutex<Vec<GuestSystemRunMetrics>>,
    shared_spawns: Arc<AtomicUsize>,
}

impl ModBudgetHook for RecordingBudgetHook {
    fn prepare_system_quotas(
        &self,
        _mod_id: Option<ModId>,
        _mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
        (
            CommandQuotaLimits::default(),
            Some(Arc::clone(&self.shared_spawns)),
        )
    }

    fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
        let n = self.run_count.fetch_add(1, Ordering::SeqCst) + 1;
        self.runs.lock().unwrap().push(metrics);
        if self.quarantine_on_run.load(Ordering::SeqCst) == n {
            BudgetWatchdogAction {
                quarantine: true,
                reason: Some(String::from("simulated p99 overrun")),
            }
        } else {
            BudgetWatchdogAction::default()
        }
    }
}

unsafe extern "C" fn drain_iter_cb(sys_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
    let mut iter: *mut BamIter = core::ptr::null_mut();
    // SAFETY: Valid active `sys_ctx` and writable `iter`.
    if unsafe { host_sys_query(sys_ctx, 0, &mut iter) }.is_ok() && !iter.is_null() {
        let mut chunk_len = 0usize;
        // SAFETY: Valid active `BamIter` and writable `chunk_len`.
        while unsafe { host_iter_next_chunk(iter, &mut chunk_len) }.get() {}
    }
    FfiResult::OK
}

#[test]
fn guest_system_budget_runtime_tracks_summary_tick_skips_and_watchdog_quarantine() {
    let mut world = World::new();
    world.insert_resource(ModQuarantine::new());
    world.insert_resource(ModHost::new());
    world.init_resource::<Messages<ModSystemFaulted>>();

    let hook = Arc::new(RecordingBudgetHook {
        quarantine_on_run: AtomicUsize::new(2),
        run_count: AtomicUsize::new(0),
        runs: Mutex::new(Vec::new()),
        shared_spawns: Arc::new(AtomicUsize::new(0)),
    });
    world.insert_resource(ModBudgetRuntime::new(
        Arc::clone(&hook) as Arc<dyn ModBudgetHook>
    ));

    let val_id = world.register_component::<SummaryVal>();
    let sparse_id = world.register_component::<SparseFlag>();
    let mut reg = ModRegistry::new();
    let val_h = reg
        .register_component_with_world_flags::<SummaryVal>(
            val_id,
            ComponentOptions::new().with_raw_insert(),
            BAM_STORAGE_FLAG_SUMMARY_TICK,
        )
        .unwrap();
    let sparse_h = reg
        .register_component::<SparseFlag>(
            sparse_id,
            ComponentOptions::new().with_raw_insert().sparse_set(),
        )
        .unwrap();
    world.insert_resource(reg.clone());

    // Spawn 2 dense entities and 1 sparse-tagged entity in separate archetypes/tables.
    world.spawn(SummaryVal(1.0));
    world.spawn(SummaryVal(2.0));
    world.spawn((SummaryVal(3.0), SparseFlag(9)));

    // Query 0 (dense): Changed<SummaryVal>; Query 1 (archetypal): Changed<SummaryVal> + Read<SparseFlag>
    let mut or_term = BamTermDesc::new(val_h, AccessMode::CHANGED);
    or_term.or_group = 1;
    let q0 = BamQuerySpecOwned::new(vec![BamTermDesc::new(val_h, AccessMode::CHANGED)]);
    let q1 = BamQuerySpecOwned::new(vec![or_term, BamTermDesc::new(sparse_h, AccessMode::READ)]);

    let mod_id = ModId(55);
    let mut sys = GuestSystem::new(
        "summary_watcher",
        vec![q0, q1],
        drain_iter_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_mod_id(mod_id)
    .with_mod_namespace("watcher_mod");

    sys.initialize(&mut world);
    assert!(
        !world
            .resource::<ModQuarantine>()
            .is_quarantined(mod_id, "summary_watcher")
    );

    // Run 1: all entities newly added -> 0 summary_tick skips, matched_tables > 0.
    sys.run((), &mut world).unwrap();
    // Run 2: nothing changed since Run 1 -> tables skipped by summary_tick AND watchdog triggers quarantine!
    sys.run((), &mut world).unwrap();
    // Run 3: quarantined -> must not invoke callback or record a 3rd run.
    sys.run((), &mut world).unwrap();

    assert!(
        world
            .resource::<ModQuarantine>()
            .is_quarantined(mod_id, "summary_watcher")
    );
    assert_eq!(world.resource::<Messages<ModSystemFaulted>>().len(), 1);

    let runs = hook.runs.lock().unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].matched_tables >= 2);
    assert_eq!(runs[0].skipped_summary_tick_tables, 0);
    assert!(runs[1].matched_tables >= 2);
    assert_eq!(runs[1].skipped_summary_tick_tables, runs[1].matched_tables);
    assert_eq!(runs[1].matched_entities, 0);
    drop(runs);

    // Re-enable via `ModQuarantine::reenable` -> Run 4 executes cleanly again.
    assert!(
        world
            .resource::<ModQuarantine>()
            .reenable(mod_id, "summary_watcher")
    );
    sys.run((), &mut world).unwrap();
    assert_eq!(hook.runs.lock().unwrap().len(), 3);
}

#[repr(C)]
#[derive(bevy_ecs::prelude::Resource, Clone, Copy, Default, bevy_abi_mod_macros::ModComponent)]
#[mod_component(name = "guest_sys_unit::TestRes")]
struct TestRes(u64);

#[repr(C)]
#[derive(Component, Clone, Copy, Default, bevy_abi_mod_macros::ModComponent)]
#[component(immutable)]
#[mod_component(name = "guest_sys_unit::ImmComp")]
struct ImmComp(u32);

struct ReasonlessQuarantineHook;

impl ModBudgetHook for ReasonlessQuarantineHook {
    fn prepare_system_quotas(
        &self,
        _mod_id: Option<ModId>,
        _mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
        (CommandQuotaLimits::default(), None)
    }

    fn record_system_run(&self, _metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
        BudgetWatchdogAction {
            quarantine: true,
            reason: None,
        }
    }
}

#[test]
#[allow(clippy::cast_ptr_alignment)]
fn guest_system_ffi_guards_random_access_policies_and_summary_or_groups() {
    use bevy_abi_mod_sys::{BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_RESOURCE, FfiBool};

    let mut world = World::new();
    world.insert_resource(ModHost::new());
    world.insert_resource(ModQuarantine::new());
    world.init_resource::<Messages<ModSystemFaulted>>();
    let val_id = world.register_component::<SummaryVal>();
    let sparse_id = world.register_component::<SparseFlag>();
    let imm_id = world.register_component::<ImmComp>();
    let res_id = world.register_component::<TestRes>();

    let mut reg = ModRegistry::new();
    let val_h = reg
        .register_component_with_world_flags::<SummaryVal>(
            val_id,
            ComponentOptions::new().with_raw_insert(),
            BAM_STORAGE_FLAG_SUMMARY_TICK,
        )
        .unwrap();
    let sparse_h = reg
        .register_component::<SparseFlag>(
            sparse_id,
            ComponentOptions::new().with_raw_insert().sparse_set(),
        )
        .unwrap();
    let imm_h = reg
        .register_component::<ImmComp>(imm_id, ComponentOptions::default())
        .unwrap();
    let res_h = reg.register_resource::<TestRes>(res_id).unwrap();

    let e_sparse = world.spawn((SummaryVal(1.0), SparseFlag(42))).id();
    let e_dense = world.spawn((SummaryVal(2.0), ImmComp(7))).id();
    let dead = world.spawn_empty().id();
    assert!(world.despawn(dead));
    let e_sparse_ffi = FfiEntity::from_bits(e_sparse.to_bits());
    let e_dense_ffi = FfiEntity::from_bits(e_dense.to_bits());

    let snap = reg.snapshot_arc();
    let iter_ptrs = [core::ptr::null_mut::<BamIter>(); MAX_SYSTEM_QUERIES];

    // 1. Guards on `HostSystemCtx` WITHOUT `world` (`ctx.world == None`) & misaligned `out_ticks`.
    let mut unaligned_buf = [0u64; 2];
    let unaligned_ticks = (&raw mut unaligned_buf)
        .cast::<u8>()
        .wrapping_add(1)
        .cast::<BamChangeTicks>();

    let plan_sparse = QueryPlan::from_terms(vec![
        crate::ffi::iter::QueryTermMeta {
            component: sparse_h,
            component_id: sparse_id,
            mode: AccessMode::ADDED,
            uuid: SparseFlag::UUID,
            size: 4,
            align: 4,
            storage_flags: bevy_abi_mod_sys::BAM_STORAGE_FLAG_SPARSE_SET,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        crate::ffi::iter::QueryTermMeta {
            component: imm_h,
            component_id: imm_id,
            mode: AccessMode::READ,
            uuid: ImmComp::UUID,
            size: 4,
            align: 4,
            storage_flags: BAM_STORAGE_FLAG_IMMUTABLE,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        crate::ffi::iter::QueryTermMeta {
            component: val_h,
            component_id: val_id,
            mode: AccessMode::FILTER_ONLY,
            uuid: SummaryVal::UUID,
            size: 4,
            align: 4,
            storage_flags: BAM_STORAGE_FLAG_SUMMARY_TICK,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
    ]);
    let plan_res = QueryPlan::from_terms(vec![
        crate::ffi::iter::QueryTermMeta {
            component: res_h,
            component_id: res_id,
            mode: AccessMode::RESOURCE_WRITE,
            uuid: TestRes::UUID,
            size: 8,
            align: 8,
            storage_flags: BAM_STORAGE_FLAG_RESOURCE,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        crate::ffi::iter::QueryTermMeta {
            component: BamComponent::from_raw(998),
            component_id: res_id,
            mode: AccessMode::RESOURCE_WRITE,
            uuid: TestRes::UUID,
            size: 8,
            align: 8,
            storage_flags: BAM_STORAGE_FLAG_RESOURCE | BAM_STORAGE_FLAG_IMMUTABLE,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
    ]);
    let mut views_no_world = [None; MAX_SYSTEM_QUERIES];
    views_no_world[0] = Some(RandomQueryView {
        plan: &plan_sparse,
        is_dense: false,
        matched_tables: &[],
        matched_archetypes: &[],
        has_query_state: true,
    });
    views_no_world[1] = Some(RandomQueryView {
        plan: &plan_res,
        is_dense: true,
        matched_tables: &[],
        matched_archetypes: &[],
        has_query_state: false,
    });
    let mut ctx_no_world = HostSystemCtx::new(
        Arc::clone(&snap),
        None,
        String::new(),
        iter_ptrs,
        2,
        core::ptr::null_mut(),
    );
    ctx_no_world.query_views = views_no_world;

    // SAFETY: Valid `HostSystemCtx` and intentionally misaligned/null pointers checked upfront.
    unsafe {
        assert!(host_sys_host_ctx(core::ptr::null()).is_null());
        assert!(!host_sys_host_ctx(ctx_no_world.as_ffi_ptr().cast_const()).is_null());

        let mut out_it = core::ptr::null_mut();
        assert_eq!(
            host_sys_query(ctx_no_world.as_ffi_ptr(), 0, core::ptr::null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_query(core::ptr::null_mut(), 0, &raw mut out_it),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_query(ctx_no_world.as_ffi_ptr(), 9, &raw mut out_it),
            FfiResult::OUT_OF_RANGE
        );
        assert_eq!(
            host_sys_query(ctx_no_world.as_ffi_ptr(), 0, &raw mut out_it),
            FfiResult::OK
        );

        let mut out_cmds = core::ptr::null_mut();
        assert_eq!(
            host_sys_commands(ctx_no_world.as_ffi_ptr(), core::ptr::null_mut()),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_commands(core::ptr::null_mut(), &raw mut out_cmds),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_sys_commands(ctx_no_world.as_ffi_ptr(), &raw mut out_cmds),
            FfiResult::OK
        );

        // `host_sys_entity_get` / `get_mut` null/invalid/world=None guards
        assert!(host_sys_entity_get(core::ptr::null(), 0, e_sparse_ffi, 0).is_null());
        assert!(
            host_sys_entity_get(ctx_no_world.as_ffi_ptr().cast_const(), 9, e_sparse_ffi, 0)
                .is_null()
        );
        assert!(
            host_sys_entity_get(
                ctx_no_world.as_ffi_ptr().cast_const(),
                0,
                FfiEntity::from_bits(1u64 << 32),
                0,
            )
            .is_null()
        );
        assert!(
            host_sys_entity_get(
                ctx_no_world.as_ffi_ptr().cast_const(),
                0,
                FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                0,
            )
            .is_null()
        );
        assert!(
            host_sys_entity_get(ctx_no_world.as_ffi_ptr().cast_const(), 0, e_sparse_ffi, 9)
                .is_null()
        );
        assert!(
            host_sys_entity_get(ctx_no_world.as_ffi_ptr().cast_const(), 0, e_sparse_ffi, 2)
                .is_null()
        );
        assert!(
            host_sys_entity_get(ctx_no_world.as_ffi_ptr().cast_const(), 0, e_sparse_ffi, 0)
                .is_null()
        );

        assert!(host_sys_entity_get_mut(core::ptr::null_mut(), 0, e_sparse_ffi, 0).is_null());
        assert!(host_sys_entity_get_mut(ctx_no_world.as_ffi_ptr(), 9, e_sparse_ffi, 0).is_null());
        assert!(
            host_sys_entity_get_mut(
                ctx_no_world.as_ffi_ptr(),
                0,
                FfiEntity::from_bits(1u64 << 32),
                0,
            )
            .is_null()
        );
        assert!(
            host_sys_entity_get_mut(
                ctx_no_world.as_ffi_ptr(),
                0,
                FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                0,
            )
            .is_null()
        );
        assert!(host_sys_entity_get_mut(ctx_no_world.as_ffi_ptr(), 0, e_sparse_ffi, 9).is_null());
        assert!(host_sys_entity_get_mut(ctx_no_world.as_ffi_ptr(), 0, e_sparse_ffi, 1).is_null());

        // `host_sys_res_ptr` / `res_mut_ptr` guards: null `sys_ctx`, misaligned `out_ticks`,
        // undeclared resource, immutable resource (`564-566`), `world == None` (`492, 569`).
        assert!(host_sys_res_ptr(core::ptr::null(), res_h, core::ptr::null_mut()).is_null());
        assert!(
            host_sys_res_ptr(
                ctx_no_world.as_ffi_ptr().cast_const(),
                res_h,
                unaligned_ticks
            )
            .is_null()
        );
        assert!(
            host_sys_res_ptr(
                ctx_no_world.as_ffi_ptr().cast_const(),
                val_h,
                core::ptr::null_mut()
            )
            .is_null()
        );
        assert!(
            host_sys_res_ptr(
                ctx_no_world.as_ffi_ptr().cast_const(),
                res_h,
                core::ptr::null_mut()
            )
            .is_null()
        );

        assert!(
            host_sys_res_mut_ptr(core::ptr::null_mut(), res_h, core::ptr::null_mut()).is_null()
        );
        assert!(host_sys_res_mut_ptr(ctx_no_world.as_ffi_ptr(), res_h, unaligned_ticks).is_null());
        assert!(
            host_sys_res_mut_ptr(ctx_no_world.as_ffi_ptr(), val_h, core::ptr::null_mut()).is_null()
        );
        assert!(
            host_sys_res_mut_ptr(
                ctx_no_world.as_ffi_ptr(),
                BamComponent::from_raw(998),
                core::ptr::null_mut()
            )
            .is_null()
        );
        assert!(
            host_sys_res_mut_ptr(ctx_no_world.as_ffi_ptr(), res_h, core::ptr::null_mut()).is_null()
        );
    }
    drop(ctx_no_world);

    // 2. World-backed `HostSystemCtx`: `!has_query_state` (`286`), sparse `ADDED` tick filter (`307`),
    // absent resource in `world` (`499`), and `get_mut` when `world == None` (`437`).
    let sparse_arch_id = world
        .archetypes()
        .iter()
        .find(|a| a.contains(sparse_id))
        .unwrap()
        .id();
    let matched_archs = [sparse_arch_id];
    let plan_sparse_write = QueryPlan::from_terms(vec![crate::ffi::iter::QueryTermMeta {
        component: sparse_h,
        component_id: sparse_id,
        mode: AccessMode::WRITE,
        uuid: SparseFlag::UUID,
        size: 4,
        align: 4,
        storage_flags: bevy_abi_mod_sys::BAM_STORAGE_FLAG_SPARSE_SET,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);
    let mut views_world = [None; MAX_SYSTEM_QUERIES];
    views_world[0] = Some(RandomQueryView {
        plan: &plan_sparse,
        is_dense: false,
        matched_tables: &[],
        matched_archetypes: &matched_archs,
        has_query_state: true,
    });
    views_world[1] = Some(RandomQueryView {
        plan: &plan_res,
        is_dense: true,
        matched_tables: &[],
        matched_archetypes: &[],
        has_query_state: false,
    });
    views_world[2] = Some(RandomQueryView {
        plan: &plan_sparse_write,
        is_dense: false,
        matched_tables: &[],
        matched_archetypes: &matched_archs,
        has_query_state: true,
    });

    let mut ctx_no_world_wr = HostSystemCtx::new(
        Arc::clone(&snap),
        None,
        String::new(),
        iter_ptrs,
        3,
        core::ptr::null_mut(),
    );
    ctx_no_world_wr.query_views = views_world;
    // SAFETY: Valid `HostSystemCtx` without `world` returns null cleanly (`437`).
    unsafe {
        assert!(
            host_sys_entity_get_mut(ctx_no_world_wr.as_ffi_ptr(), 2, e_sparse_ffi, 0).is_null()
        );
    }
    drop(ctx_no_world_wr);

    let mut ctx_w = HostSystemCtx::new(
        Arc::clone(&snap),
        None,
        String::new(),
        iter_ptrs,
        3,
        core::ptr::null_mut(),
    )
    .with_world_and_queries(
        world.as_unsafe_world_cell_readonly(),
        views_world,
        Tick::new(0),
        Tick::new(10),
    );
    // SAFETY: Valid world-backed `HostSystemCtx`.
    unsafe {
        // `!has_query_state` on query 1 (`286`)
        assert!(host_sys_entity_get(ctx_w.as_ffi_ptr().cast_const(), 1, e_sparse_ffi, 0).is_null());
        // Sparse `ADDED` tick filter (`307`): `e_sparse` was newly added -> returns non-null; `e_dense` lacks `SparseFlag` -> null.
        assert!(
            !host_sys_entity_get(ctx_w.as_ffi_ptr().cast_const(), 0, e_sparse_ffi, 0).is_null()
        );
        assert!(host_sys_entity_get(ctx_w.as_ffi_ptr().cast_const(), 0, e_dense_ffi, 0).is_null());
        // Absent `TestRes` in `world` -> returns null (`499`, `573`)
        let mut ticks = BamChangeTicks {
            added: 0,
            changed: 0,
            last_run: 0,
            this_run: 0,
        };
        assert!(host_sys_res_ptr(ctx_w.as_ffi_ptr().cast_const(), res_h, &raw mut ticks).is_null());
        assert!(host_sys_res_mut_ptr(ctx_w.as_ffi_ptr(), res_h, &raw mut ticks).is_null());
    }
    drop(ctx_w);

    // Insert `TestRes` and verify `host_sys_res_ptr` & `host_sys_res_mut_ptr` populate `ticks`.
    world.insert_resource(TestRes(77));
    let mut ctx_w2 = HostSystemCtx::new(
        Arc::clone(&snap),
        None,
        String::new(),
        iter_ptrs,
        3,
        core::ptr::null_mut(),
    )
    .with_world_and_queries(
        world.as_unsafe_world_cell(),
        views_world,
        Tick::new(0),
        Tick::new(10),
    );
    let mut ticks = BamChangeTicks {
        added: 0,
        changed: 0,
        last_run: 0,
        this_run: 0,
    };
    // SAFETY: Valid world-backed `HostSystemCtx` and aligned `ticks`.
    unsafe {
        assert!(
            !host_sys_res_ptr(ctx_w2.as_ffi_ptr().cast_const(), res_h, &raw mut ticks).is_null()
        );
        assert!(!host_sys_res_mut_ptr(ctx_w2.as_ffi_ptr(), res_h, &raw mut ticks).is_null());
    }
    drop(ctx_w2);

    // 3. `table_skipped_by_summary_tick` `or_group` branches (`FILTER_ONLY`, `WITHOUT`, `ADDED`, `CHANGED`),
    // `run_if` returning `false`, `validate_against_registry`, `DelegateToBevyFallback`,
    // `ReasonlessQuarantineHook` (`reason: None`), and `System::initialize` error policy paths.
    let mut or_f = BamTermDesc::new(imm_h, AccessMode::FILTER_ONLY);
    or_f.or_group = 1;
    let mut or_w = BamTermDesc::new(sparse_h, AccessMode::WITHOUT);
    or_w.or_group = 2;
    let mut or_c = BamTermDesc::new(val_h, AccessMode::CHANGED);
    or_c.or_group = 3;
    let mut or_a = BamTermDesc::new(imm_h, AccessMode::ADDED);
    or_a.or_group = 3;
    let complex_spec = BamQuerySpecOwned::new(vec![or_f, or_w, or_c, or_a]);

    world.insert_resource(ModBudgetRuntime::new(Arc::new(ReasonlessQuarantineHook)));

    unsafe extern "C" fn cond_false(_ud: *mut c_void) -> FfiBool {
        FfiBool::FALSE
    }
    unsafe extern "C" fn fail_cb(_ctx: *mut BamSystemCtx, _ud: *mut c_void) -> FfiResult {
        FfiResult::INVALID_ARGUMENT
    }

    let mut sys = GuestSystem::new(
        "complex_summary_or",
        vec![complex_spec],
        drain_iter_cb,
        core::ptr::null_mut(),
        reg.clone(),
    )
    .with_mod_id(ModId(77))
    .with_mod_namespace("or_ns")
    .with_weak_ordering(vec![String::from("a")], vec![String::from("b")])
    .with_in_sets(vec![String::from("s")])
    .with_ambiguous_with(vec![String::from("c")])
    .with_ambiguous_with_all(true)
    .with_run_if(Some((cond_false, core::ptr::null_mut())));
    assert_eq!(sys.after_weak_systems(), &[String::from("a")]);
    assert_eq!(sys.before_weak_systems(), &[String::from("b")]);
    assert_eq!(sys.in_sets(), &[String::from("s")]);
    assert_eq!(sys.ambiguous_with_systems(), &[String::from("c")]);
    assert!(sys.is_ambiguous_with_all());
    assert!(sys.run_if().is_some());
    assert!(sys.validate_against_registry(&reg).is_ok());

    sys.initialize(&mut world);
    // `run_if` returns false -> callback skipped without quarantining.
    sys.run((), &mut world).unwrap();
    assert!(sys.is_enabled());

    // Clear `run_if` and run once: `ReasonlessQuarantineHook` quarantines with `reason == None` (`1547-1548`).
    sys.set_run_if(None);
    sys.run((), &mut world).unwrap();
    assert!(!sys.is_enabled());
    world.check_change_ticks();
    let prev = sys.get_last_run();
    sys.set_last_run(prev);

    // `validate_against_registry` error + `System::initialize` error with `ReportToBevy`
    let mut bad_sys = GuestSystem::new(
        "bad_sys",
        vec![BamQuerySpecOwned::new(vec![BamTermDesc::new(
            BamComponent::from_raw(999),
            AccessMode::READ,
        )])],
        fail_cb,
        core::ptr::null_mut(),
        reg.clone(),
    )
    .with_error_policy(ModErrorPolicy::ReportToBevy);
    assert!(bad_sys.validate_against_registry(&reg).is_err());
    bad_sys.initialize(&mut world);
    assert!(bad_sys.init_error().is_some());

    // `DelegateToBevyFallback` runtime fault returns `Err(RunSystemError::Failed(_))`
    // without quarantining (`enabled_flag` stays true).
    let mut fault_sys = GuestSystem::new(
        "fault_delegate",
        Vec::new(),
        fail_cb,
        core::ptr::null_mut(),
        reg,
    )
    .with_error_policy(ModErrorPolicy::DelegateToBevyFallback);
    fault_sys.initialize(&mut world);
    assert!(fault_sys.run((), &mut world).is_err());
    assert!(fault_sys.is_enabled());

    #[cfg(feature = "diagnostics")]
    {
        let col = ModDiagnosticsCollector::new();
        col.push(ModDiagnosticsSample {
            mod_id: Some(ModId(1)),
            mod_name: Arc::from("m"),
            system_name: Arc::from("s"),
            cpu_time_us: 1.5,
            commands_queued: 0,
            faulted: false,
        });
        assert_eq!(col.drain().len(), 1);
    }
}
