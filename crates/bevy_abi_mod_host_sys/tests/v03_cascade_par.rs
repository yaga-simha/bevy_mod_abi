//! ABI `0.3.0` Phase 2 integration tests for:
//! - Lazy per-`(World, RelationComponentId)` [`CascadeHierarchyCache`] (50,000-entity 6-depth
//!   transform cascade, parent-before-child ordering, steady-state cache reuse across unchanged
//!   ticks, automatic invalidation on `Changed` / `RemovedComponents`, custom 4-byte/8-byte &
//!   dynamic relationships, and cycle guards).
//! - [`host_iter_up_cascade`] ancestor lookup, default `ChildOf` vs explicit/custom relations,
//!   `NOT_FOUND` zeroing, permission enforcement, and cycle safety.
//! - Zero-arena multi-core [`host_par_for_each_chunk_readonly`] bit-exact parity vs sequential
//!   iteration, `PERMISSION_DENIED` on writable queries, atomic error propagation, non-reentrancy,
//!   and verified zero per-worker heap/command-arena allocations.

use bevy_abi_mod_host_sys::{
    CascadeHierarchyCache, ComponentOptions, HOST_API, HostIter, LoadContext, ModId, ModRegistry,
    QueryPlan, QueryTermMeta, SparseRowScratch, ensure_dynamic_hook_map_in_world,
    host_iter_column_info, host_iter_column_readonly, host_iter_column_rows_readonly,
    host_iter_count, host_iter_entities, host_iter_next_chunk, host_iter_up_cascade,
    host_par_for_each_chunk, host_par_for_each_chunk_readonly, host_register_component,
    host_register_system, host_res_ptr, host_sys_query,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BamColumnInfo, BamComponent, BamIter, BamQueryDesc, BamSystemCtx, BamSystemDesc, BamTermDesc,
    ComponentDesc, ComponentUuid, FfiBool, FfiEntity, FfiResult, ModComponent as _,
};
use bevy_ecs::{
    change_detection::Tick, component::Component, entity::Entity, hierarchy::ChildOf,
    prelude::Resource, ptr::OwningPtr, schedule::Schedule, world::World,
};
use bevy_platform::collections::HashMap;
use core::alloc::{GlobalAlloc, Layout};
use core::cell::Cell;
use core::ffi::c_void;
use core::ptr::{null, null_mut};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::alloc::System;
use std::sync::Arc;

// ============================================================================
// Global Allocation Counter for Zero-Arena / Zero-Worker-Heap Verification
// ============================================================================

struct CountingAlloc;

thread_local! {
    static TRACK_WORKER_ALLOCS: Cell<bool> = const { Cell::new(false) };
}

static WORKER_ALLOC_COUNT: AtomicUsize = AtomicUsize::new(0);
static WORKER_ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);

// SAFETY: Delegates all allocation/deallocation directly to `std::alloc::System`, only bumping
// lock-free atomic counters when the current thread explicitly enables `TRACK_WORKER_ALLOCS`.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = TRACK_WORKER_ALLOCS.try_with(|flag| {
            if flag.get() {
                WORKER_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                WORKER_ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            }
        });
        // SAFETY: Forwarding directly to `std::alloc::System::alloc`.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: Forwarding directly to `std::alloc::System::dealloc`.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let _ = TRACK_WORKER_ALLOCS.try_with(|flag| {
            if flag.get() {
                WORKER_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                WORKER_ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
            }
        });
        // SAFETY: Forwarding directly to `std::alloc::System::alloc_zeroed`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = TRACK_WORKER_ALLOCS.try_with(|flag| {
            if flag.get() {
                WORKER_ALLOC_COUNT.fetch_add(1, Ordering::Relaxed);
                WORKER_ALLOC_BYTES.fetch_add(new_size, Ordering::Relaxed);
            }
        });
        // SAFETY: Forwarding directly to `std::alloc::System::realloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL_ALLOC: CountingAlloc = CountingAlloc;

struct WorkerAllocGuard;

impl WorkerAllocGuard {
    fn enter() -> Self {
        TRACK_WORKER_ALLOCS.with(|f| f.set(true));
        Self
    }
}

impl Drop for WorkerAllocGuard {
    fn drop(&mut self) {
        TRACK_WORKER_ALLOCS.with(|f| f.set(false));
    }
}

// ============================================================================
// Test Components & Resources
// ============================================================================

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::LocalOffset")]
struct LocalOffset {
    dx: i64,
    dy: i64,
    dz: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::WorldTransform")]
struct WorldTransform {
    x: i64,
    y: i64,
    z: i64,
    eval_seq: u64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::ThemeColor")]
struct ThemeColor {
    rgba: u32,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::OptBonus")]
struct OptBonus {
    bonus: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::TagA")]
struct TagA;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[component(storage = "SparseSet")]
#[mod_component(name = "v03_cp::SparseWeight")]
struct SparseWeight {
    w: i64,
}

#[repr(C)]
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "v03_cp::ScaleRes")]
struct ScaleRes {
    factor: i64,
}

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct CustomLink8(FfiEntity);

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, PartialEq)]
struct CustomLink4(u32);

/// Helper applying a single-pass breadth-first transform cascade using [`CascadeHierarchyCache`].
fn run_single_pass_transform_cascade(
    world: &mut World,
    cache: &CascadeHierarchyCache,
    child_of_id: bevy_ecs::component::ComponentId,
) {
    let mut seq = 1u64;
    cache.for_each_cascade(child_of_id, |_depth, entity, parent| {
        let local = *world.entity(entity).get::<LocalOffset>().unwrap();
        let (px, py, pz) = match parent {
            Some(p) => {
                let p_tf = *world.entity(p).get::<WorldTransform>().unwrap();
                assert!(
                    p_tf.eval_seq > 0 && p_tf.eval_seq < seq,
                    "parent {p:?} (seq {}) must be evaluated strictly before child {entity:?} (seq {seq})",
                    p_tf.eval_seq
                );
                (p_tf.x, p_tf.y, p_tf.z)
            }
            None => (0, 0, 0),
        };
        let mut wt = world.entity_mut(entity).into_mut::<WorldTransform>().unwrap();
        wt.x = px + local.dx;
        wt.y = py + local.dy;
        wt.z = pz + local.dz;
        wt.eval_seq = seq;
        seq += 1;
    });
}

/// Wrapper permitting disjoint per-stratum parallel reads of parents (completed prior stratum) and
/// writes to disjoint child entities (current stratum) across `ComputeTaskPool`.
#[derive(Clone, Copy)]
struct SyncWorldCell<'w>(bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell<'w>);

// SAFETY: Used strictly inside `CascadeHierarchyCache::par_for_each_stratum` where:
// 1. All entities at depth `d - 1` (parents) finished writing before stratum `d` begins.
// 2. All entities within stratum `d` are distinct and partitioned into disjoint slices, so each
//    worker exclusively mutates `WorldTransform` on its own disjoint slice while only reading
//    completed parents from prior strata and read-only `LocalOffset`.
unsafe impl Sync for SyncWorldCell<'_> {}
// SAFETY: Same strict stratum-barrier and disjoint-slice invariants as `Sync`.
unsafe impl Send for SyncWorldCell<'_> {}

// ============================================================================
// 1. 50,000-Entity 6-Depth Transform Cascade + Cache Reuse + Invalidation
// ============================================================================

#[test]
fn cascade_50k_entities_6_depths_parent_before_child_reuse_and_invalidation() {
    let mut world = World::new();

    // Build an exact 50,000-entity forest across 6 topological depths (0..=5):
    // Depth 0:     20 roots
    // Depth 1:    100 (5 per depth-0 root)
    // Depth 2:    500 (5 per depth-1 node)
    // Depth 3:  2,500 (5 per depth-2 node)
    // Depth 4: 12,500 (5 per depth-3 node)
    // Depth 5: 34,380 (distributed round-robin across depth-4 nodes)
    // Total  = 50,000 entities.
    let mut levels: [Vec<Entity>; 6] = Default::default();
    let mut expected_parent: HashMap<Entity, Option<Entity>> = HashMap::with_capacity(50_000);
    let mut local_map: HashMap<Entity, LocalOffset> = HashMap::with_capacity(50_000);

    for i in 0..20i64 {
        let local = LocalOffset {
            dx: i + 1,
            dy: (i + 1) * 2,
            dz: (i + 1) * 3,
        };
        let root = world.spawn((local, WorldTransform::default())).id();
        levels[0].push(root);
        expected_parent.insert(root, None);
        local_map.insert(root, local);
    }

    for depth in 1..=4usize {
        let parents = levels[depth - 1].clone();
        levels[depth].reserve(parents.len() * 5);
        for (p_idx, &parent) in parents.iter().enumerate() {
            for k in 0..5i64 {
                let local = LocalOffset {
                    dx: (depth as i64) * 10 + k,
                    dy: ((p_idx as i64) % 7) - 3,
                    dz: k + 1,
                };
                let child = world
                    .spawn((local, WorldTransform::default(), ChildOf(parent)))
                    .id();
                levels[depth].push(child);
                expected_parent.insert(child, Some(parent));
                local_map.insert(child, local);
            }
        }
    }

    let d4_len = levels[4].len();
    levels[5].reserve(34_380);
    for i in 0..34_380usize {
        let parent = levels[4][i % d4_len];
        let local = LocalOffset {
            dx: 50 + (i % 11) as i64,
            dy: -((i % 5) as i64),
            dz: 1 + (i % 3) as i64,
        };
        let leaf = world
            .spawn((local, WorldTransform::default(), ChildOf(parent)))
            .id();
        levels[5].push(leaf);
        expected_parent.insert(leaf, Some(parent));
        local_map.insert(leaf, local);
    }

    let total_spawned: usize = levels.iter().map(Vec::len).sum();
    assert_eq!(total_spawned, 50_000);

    let child_of_id = world.components().component_id::<ChildOf>().unwrap();
    let mut cache = CascadeHierarchyCache::new();

    // Initial build populates all 6 depth strata cleanly.
    assert!(cache.ensure_built(&world, child_of_id));
    assert_eq!(cache.rebuild_count(child_of_id), 1);
    assert!(!cache.has_cycles(child_of_id));
    assert_eq!(cache.strata(child_of_id).len(), 6);
    assert_eq!(cache.strata(child_of_id)[0].len(), 20);
    assert_eq!(cache.strata(child_of_id)[1].len(), 100);
    assert_eq!(cache.strata(child_of_id)[2].len(), 500);
    assert_eq!(cache.strata(child_of_id)[3].len(), 2_500);
    assert_eq!(cache.strata(child_of_id)[4].len(), 12_500);
    assert_eq!(cache.strata(child_of_id)[5].len(), 34_380);

    // 1. Single-pass cascade validates strict parent-before-child order across all 50,000 entities.
    run_single_pass_transform_cascade(&mut world, &cache, child_of_id);

    // Compute ground-truth transforms and verify all 50,000 entities match bit-for-bit.
    let mut truth: HashMap<Entity, (i64, i64, i64)> = HashMap::with_capacity(50_000);
    for (depth, level) in levels.iter().enumerate() {
        for &e in level {
            let loc = local_map[&e];
            let (px, py, pz) = match expected_parent[&e] {
                Some(p) => truth[&p],
                None => (0, 0, 0),
            };
            let exp = (px + loc.dx, py + loc.dy, pz + loc.dz);
            truth.insert(e, exp);
            let actual = *world.entity(e).get::<WorldTransform>().unwrap();
            assert_eq!(
                (actual.x, actual.y, actual.z),
                exp,
                "single-pass mismatch at depth {depth} for {e:?}"
            );
            assert_eq!(cache.depth_of(child_of_id, e), Some(depth));
            assert_eq!(cache.parent_of(child_of_id, e), expected_parent[&e]);
        }
    }

    // 2. Per-stratum parallel cascade across `ComputeTaskPool` produces bit-exact parity!
    for level in &levels {
        for &e in level {
            *world.entity_mut(e).get_mut::<WorldTransform>().unwrap() = WorldTransform::default();
        }
    }
    {
        let cell = SyncWorldCell(world.as_unsafe_world_cell());
        let wt_cid = cell
            .0
            .components()
            .component_id::<WorldTransform>()
            .unwrap();
        let lo_cid = cell.0.components().component_id::<LocalOffset>().unwrap();
        let par_seq = AtomicU64::new(1);

        cache.par_for_each_stratum(child_of_id, |depth, chunk| {
            let w = cell.0;
            for &entity in chunk {
                let ent_cell = w.get_entity(entity).expect("live entity");
                // SAFETY: `LocalOffset` is read-only across all strata during this parallel cascade.
                let local = unsafe { *ent_cell.get_by_id(lo_cid).unwrap().deref::<LocalOffset>() };
                let (px, py, pz) = match cache.parent_of(child_of_id, entity) {
                    Some(parent) => {
                        let p_cell = w.get_entity(parent).expect("live parent");
                        // SAFETY: Parent resides in stratum `depth - 1`, which joined completely
                        // before stratum `depth` started; read-only shared borrow is race-free.
                        let p_tf =
                            unsafe { *p_cell.get_by_id(wt_cid).unwrap().deref::<WorldTransform>() };
                        assert!(
                            p_tf.eval_seq > 0,
                            "stratum barrier violated: parent {parent:?} unvisited at depth {depth}"
                        );
                        (p_tf.x, p_tf.y, p_tf.z)
                    }
                    None => (0, 0, 0),
                };
                // SAFETY: Entities in `chunk` are strictly unique within stratum `depth` and
                // disjoint across parallel workers, granting exclusive access to `entity`'s
                // `WorldTransform`.
                let mut wt = unsafe {
                    ent_cell
                        .get_mut_by_id(wt_cid)
                        .unwrap()
                        .with_type::<WorldTransform>()
                };
                wt.x = px + local.dx;
                wt.y = py + local.dy;
                wt.z = pz + local.dz;
                wt.eval_seq = par_seq.fetch_add(1, Ordering::Relaxed);
            }
        });
    }
    for level in &levels {
        for &e in level {
            let actual = *world.entity(e).get::<WorldTransform>().unwrap();
            assert_eq!((actual.x, actual.y, actual.z), truth[&e]);
            assert!(actual.eval_seq > 0);
        }
    }

    // 3. Steady-state cache reuse across 5 unmodified ticks (even while mutating `LocalOffset`!).
    for frame in 0..5 {
        world
            .entity_mut(levels[0][0])
            .get_mut::<LocalOffset>()
            .unwrap()
            .dx += 1;
        world.increment_change_tick();
        assert!(
            !cache.ensure_built(&world, child_of_id),
            "cache must NOT rebuild on unmodified frame {frame}"
        );
    }
    assert_eq!(cache.rebuild_count(child_of_id), 1);

    // 4. Automatic invalidation when a `ChildOf` edge mutates (reparent depth-2 subtree under root 1).
    let moved_node = levels[2][0];
    let moved_child = levels[3][0]; // direct child of `moved_node`
    let new_root = levels[0][1];
    assert_eq!(cache.parent_of(child_of_id, moved_child), Some(moved_node));
    world.entity_mut(moved_node).insert(ChildOf(new_root));

    assert!(
        cache.ensure_built(&world, child_of_id),
        "cache MUST invalidate when a ChildOf edge mutates"
    );
    assert_eq!(cache.rebuild_count(child_of_id), 2);
    assert_eq!(cache.parent_of(child_of_id, moved_node), Some(new_root));
    assert_eq!(cache.depth_of(child_of_id, moved_node), Some(1));
    assert_eq!(cache.depth_of(child_of_id, moved_child), Some(2));
    // Re-running without further hierarchy edits hits the cache again!
    assert!(!cache.ensure_built(&world, child_of_id));
    assert_eq!(cache.rebuild_count(child_of_id), 2);

    // 5. Automatic invalidation when a `ChildOf` edge is removed (`RemovedComponents`).
    world.entity_mut(moved_node).remove::<ChildOf>();
    assert!(
        cache.ensure_built(&world, child_of_id),
        "cache MUST invalidate when a ChildOf edge is removed"
    );
    assert_eq!(cache.rebuild_count(child_of_id), 3);
    assert_eq!(cache.parent_of(child_of_id, moved_node), None);
    assert_eq!(cache.depth_of(child_of_id, moved_node), Some(0));
    assert_eq!(cache.depth_of(child_of_id, moved_child), Some(1));
    assert_eq!(cache.strata(child_of_id)[0].len(), 21);

    // Re-run single-pass cascade after detachment and verify `moved_node` now uses its own local
    // offset without `new_root`'s offset.
    run_single_pass_transform_cascade(&mut world, &cache, child_of_id);
    let moved_loc = *world.entity(moved_node).get::<LocalOffset>().unwrap();
    let moved_wt = *world.entity(moved_node).get::<WorldTransform>().unwrap();
    assert_eq!(
        (moved_wt.x, moved_wt.y, moved_wt.z),
        (moved_loc.dx, moved_loc.dy, moved_loc.dz)
    );
    let child_loc = *world.entity(moved_child).get::<LocalOffset>().unwrap();
    let child_wt = *world.entity(moved_child).get::<WorldTransform>().unwrap();
    assert_eq!(
        (child_wt.x, child_wt.y, child_wt.z),
        (
            moved_loc.dx + child_loc.dx,
            moved_loc.dy + child_loc.dy,
            moved_loc.dz + child_loc.dz
        )
    );
}

// ============================================================================
// 2. `host_iter_up_cascade` Ancestor Lookup, Custom Relations, NOT_FOUND & Cycle Guard
// ============================================================================

#[test]
fn iter_up_cascade_ancestors_not_found_custom_relations_and_cycle_guard() {
    let mut world = World::new();
    ensure_dynamic_hook_map_in_world(&mut world);

    let theme_cid = world.register_component::<ThemeColor>();
    let child_of_cid = world.register_component::<ChildOf>();
    let link8_cid = world.register_component::<CustomLink8>();
    let link4_cid = world.register_component::<CustomLink4>();

    let mut reg = ModRegistry::new();
    let theme_h = reg
        .register_component::<ThemeColor>(theme_cid, ComponentOptions::default())
        .unwrap();
    let child_of_h = reg.register_hierarchy(child_of_cid, true).unwrap();
    world.insert_resource(reg.clone());

    // Register a dynamic relationship (`DynParent` / `DynChildren`) too!
    let mut dyn_rel_h = BamComponent::INVALID;
    let mut dyn_tgt_h = BamComponent::INVALID;
    let (dyn_rel_cid, updated_reg) = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &raw mut world,
            Some(ModId(41)),
        );
        let rel_desc = ComponentDesc::new(
            c"v03_cp::DynParent".as_ptr(),
            ComponentUuid::from_parts("v03_cp::DynParent", 8, 8, 0),
            8,
            8,
        );
        let tgt_desc = ComponentDesc::new(
            c"v03_cp::DynChildren".as_ptr(),
            ComponentUuid::from_parts("v03_cp::DynChildren", 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &rel_desc, &raw mut dyn_rel_h),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &tgt_desc, &raw mut dyn_tgt_h),
                FfiResult::OK
            );
        }
        let bind_name =
            std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", dyn_rel_h.0, dyn_tgt_h.0))
                .unwrap();
        let bind_desc = ComponentDesc::new(
            bind_name.as_ptr(),
            ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        let mut bind_out = BamComponent::INVALID;
        // SAFETY: Valid `LoadContext` and `__bam_rel_bind` descriptor.
        let rb =
            unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &raw mut bind_out) };
        assert_eq!(rb, FfiResult::OK);
        let r_cid = ctx.registry.get_by_handle(dyn_rel_h).unwrap().component_id;
        let out_reg = ctx.registry.clone();
        ctx.finish_init();
        (r_cid, out_reg)
    };
    world.insert_resource(updated_reg);

    // Hierarchy:
    //   root (ThemeColor 0xAABBCCDD)
    //     └─ mid1 (no ThemeColor)
    //          └─ mid2 (ThemeColor 0x11223344)
    //               └─ mid3 (no ThemeColor)
    //                    └─ leaf (ThemeColor 0x99887766)
    //   orphan_root (no ThemeColor) -> orphan_child (no ThemeColor)
    let root = world.spawn(ThemeColor { rgba: 0xAABB_CCDD }).id();
    let mid1 = world
        .spawn((
            ChildOf(root),
            CustomLink8(FfiEntity::from_bits(root.to_bits())),
            CustomLink4(root.index_u32()),
        ))
        .id();
    let mid2 = world
        .spawn((
            ThemeColor { rgba: 0x1122_3344 },
            ChildOf(mid1),
            CustomLink8(FfiEntity::from_bits(mid1.to_bits())),
            CustomLink4(mid1.index_u32()),
        ))
        .id();
    let mid3 = world
        .spawn((
            ChildOf(mid2),
            CustomLink8(FfiEntity::from_bits(mid2.to_bits())),
            CustomLink4(mid2.index_u32()),
        ))
        .id();
    let leaf = world
        .spawn((
            ThemeColor { rgba: 0x9988_7766 },
            ChildOf(mid3),
            CustomLink8(FfiEntity::from_bits(mid3.to_bits())),
            CustomLink4(mid3.index_u32()),
        ))
        .id();
    let orphan_root = world.spawn_empty().id();
    let orphan_child = world.spawn(ChildOf(orphan_root)).id();

    // Attach dynamic relationship `leaf -> mid1 -> root` (`mid1` lacks `ThemeColor`, `root` has it).
    let mut root_bits = root.to_bits();
    let mut mid1_bits = mid1.to_bits();
    // SAFETY: `dyn_rel_cid` has `(8, 8)` `FfiEntity` layout.
    unsafe {
        world.entity_mut(mid1).insert_by_id(
            dyn_rel_cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut root_bits).cast()),
        );
        world.entity_mut(leaf).insert_by_id(
            dyn_rel_cid,
            OwningPtr::new(core::ptr::NonNull::from(&mut mid1_bits).cast()),
        );
    }

    // Malformed cycle: `cyc_a -> cyc_b -> cyc_c -> cyc_a` (none has `ThemeColor`).
    let cyc_a = world.spawn_empty().id();
    let cyc_b = world.spawn_empty().id();
    let cyc_c = world.spawn_empty().id();
    world
        .entity_mut(cyc_a)
        .insert(CustomLink8(FfiEntity::from_bits(cyc_b.to_bits())));
    world
        .entity_mut(cyc_b)
        .insert(CustomLink8(FfiEntity::from_bits(cyc_c.to_bits())));
    world
        .entity_mut(cyc_c)
        .insert(CustomLink8(FfiEntity::from_bits(cyc_a.to_bits())));

    let link8_h = BamComponent::from_raw(501);
    let link4_h = BamComponent::from_raw(502);
    let plan = QueryPlan::from_terms(vec![
        QueryTermMeta {
            component: theme_h,
            component_id: theme_cid,
            mode: AccessMode::READ,
            uuid: ThemeColor::UUID,
            size: core::mem::size_of::<ThemeColor>(),
            align: core::mem::align_of::<ThemeColor>(),
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: link8_h,
            component_id: link8_cid,
            mode: AccessMode::READ,
            uuid: ComponentUuid::from_canonical_name("v03_cp::CustomLink8"),
            size: 8,
            align: 8,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: link4_h,
            component_id: link4_cid,
            mode: AccessMode::READ,
            uuid: ComponentUuid::from_canonical_name("v03_cp::CustomLink4"),
            size: 4,
            align: 4,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
    ]);

    let mut iter = HostIter::new_dense(
        &world.storages().tables,
        &[],
        &plan,
        Tick::new(0),
        Tick::new(1),
    )
    .with_world(world.as_unsafe_world_cell_readonly());

    let mut out_ptr: *const c_void = null();
    let mut out_owner = FfiEntity::NULL;

    // SAFETY: `iter` and output buffers are valid and aligned.
    unsafe {
        // 1. Default `ChildOf` (`BamComponent::INVALID`): `leaf` -> skips `mid3` -> finds `mid2`.
        assert_eq!(
            host_iter_up_cascade(
                iter.as_ffi_ptr(),
                BamComponent::INVALID,
                FfiEntity::from_bits(leaf.to_bits()),
                0,
                &raw mut out_ptr,
                &raw mut out_owner,
            ),
            FfiResult::OK
        );
        assert_eq!(out_owner, FfiEntity::from_bits(mid2.to_bits()));
        assert_eq!((*out_ptr.cast::<ThemeColor>()).rgba, 0x1122_3344);

        // 2. Explicit `ChildOf` handle: `mid2` -> skips `mid1` -> finds `root`.
        assert_eq!(
            host_iter_up_cascade(
                iter.as_ffi_ptr(),
                child_of_h,
                FfiEntity::from_bits(mid2.to_bits()),
                0,
                &raw mut out_ptr,
                &raw mut out_owner,
            ),
            FfiResult::OK
        );
        assert_eq!(out_owner, FfiEntity::from_bits(root.to_bits()));
        assert_eq!((*out_ptr.cast::<ThemeColor>()).rgba, 0xAABB_CCDD);

        // 3. Custom 8-byte and 4-byte relations yield identical ancestor resolution.
        for rel in [link8_h, link4_h] {
            assert_eq!(
                host_iter_up_cascade(
                    iter.as_ffi_ptr(),
                    rel,
                    FfiEntity::from_bits(leaf.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(mid2.to_bits()));
            assert_eq!((*out_ptr.cast::<ThemeColor>()).rgba, 0x1122_3344);
        }

        // 4. `DynamicComponentHookMap` relationship (source AND target handles!) walks `leaf -> mid1 -> root`.
        for rel in [dyn_rel_h, dyn_tgt_h] {
            assert_eq!(
                host_iter_up_cascade(
                    iter.as_ffi_ptr(),
                    rel,
                    FfiEntity::from_bits(leaf.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(root.to_bits()));
            assert_eq!((*out_ptr.cast::<ThemeColor>()).rgba, 0xAABB_CCDD);
        }

        // 5. `NOT_FOUND` on root & orphan subtree zeroes `*out_ptr` and `*out_owner`.
        for ent in [root, orphan_child] {
            assert_eq!(
                host_iter_up_cascade(
                    iter.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(ent.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NOT_FOUND
            );
            assert!(out_ptr.is_null());
            assert_eq!(out_owner, FfiEntity::NULL);
        }

        // 6. 3-node cycle guard terminates cleanly with `NOT_FOUND` and zeroed outputs.
        assert_eq!(
            host_iter_up_cascade(
                iter.as_ffi_ptr(),
                link8_h,
                FfiEntity::from_bits(cyc_a.to_bits()),
                0,
                &raw mut out_ptr,
                &raw mut out_owner,
            ),
            FfiResult::NOT_FOUND
        );
        assert!(out_ptr.is_null());
        assert_eq!(out_owner, FfiEntity::NULL);
    }
}

// ============================================================================
// 3. Multi-Core `host_par_for_each_chunk_readonly`: Parity, Permissions, Errors & Zero-Alloc
// ============================================================================

struct ParParityAcc {
    entity_count: AtomicUsize,
    chunk_count: AtomicUsize,
    xor_hash: AtomicU64,
    sum_hash: AtomicU64,
    has_true_count: AtomicUsize,
    nested_par_ro_rc: AtomicUsize,
    nested_par_rc: AtomicUsize,
    track_allocs: bool,
    fail_on_chunk_threshold: usize,
}

#[inline]
fn fold_row_hash(
    ent_bits: u64,
    off: LocalOffset,
    opt_bonus: i64,
    has_tag: bool,
    scale: i64,
) -> u64 {
    let mut h = ent_bits.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    h ^= (off.dx as u64).rotate_left(7);
    h = h.wrapping_add((off.dy as u64).wrapping_mul(31));
    h ^= (off.dz as u64).rotate_left(19);
    h = h.wrapping_add((opt_bonus as u64).wrapping_mul(131));
    h ^= u64::from(has_tag).rotate_left(3);
    h.wrapping_mul((scale as u64) | 1)
}

unsafe extern "C" fn noop_task(_iter: *mut BamIter, _ud: *mut c_void) -> FfiResult {
    FfiResult::OK
}

unsafe extern "C" fn dense_parity_worker(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
    // SAFETY: `ud` points to a live `ParParityAcc` for the synchronous duration of dispatch.
    let acc = unsafe { &*ud.cast::<ParParityAcc>() };
    let _alloc_guard = acc.track_allocs.then(WorkerAllocGuard::enter);

    // Non-reentrant checks: both `par_for_each_chunk_readonly` and `par_for_each_chunk` reject nested calls.
    // SAFETY: `sub_iter` is an active worker `BamIter`.
    let nested_ro = unsafe { host_par_for_each_chunk_readonly(sub_iter, Some(noop_task), ud) };
    // SAFETY: `sub_iter` is an active worker `BamIter`.
    let nested_rw = unsafe { host_par_for_each_chunk(sub_iter, Some(noop_task), ud) };
    acc.nested_par_ro_rc
        .store(nested_ro.0 as usize, Ordering::Relaxed);
    acc.nested_par_rc
        .store(nested_rw.0 as usize, Ordering::Relaxed);

    // Column info metadata check (term 0 = LocalOffset).
    let mut info = BamColumnInfo {
        component: BamComponent::INVALID,
        mode: AccessMode::READ,
        uuid: ComponentUuid::from_bytes([0; 16]),
        size: 0,
        align: 0,
        storage_flags: 0,
        reserved0: 0,
    };
    // SAFETY: `sub_iter` and `info` are valid.
    let info_rc = unsafe { host_iter_column_info(sub_iter.cast_const(), 0, &raw mut info) };
    if info_rc.is_err()
        || info.uuid != LocalOffset::UUID
        || info.size != core::mem::size_of::<LocalOffset>()
    {
        return FfiResult::TYPE_MISMATCH;
    }

    // Read-only singleton resource lookup (term 3 = ScaleRes).
    // SAFETY: `sub_iter` carries read-only `ScaleRes` at term 3.
    let res_ptr = unsafe { host_res_ptr(sub_iter.cast_const(), 3) };
    if res_ptr.is_null() {
        return FfiResult::NOT_FOUND;
    }
    // SAFETY: `res_ptr` points to live, aligned `ScaleRes`.
    #[allow(clippy::cast_ptr_alignment)]
    let scale = unsafe { (*res_ptr.cast::<ScaleRes>()).factor };

    let mut sub_count = 0usize;
    // SAFETY: `sub_iter` and `sub_count` are valid.
    let count_rc = unsafe { host_iter_count(sub_iter, &raw mut sub_count) };
    if count_rc.is_err() {
        return FfiResult::INTERNAL_ERROR;
    }

    let mut len = 0usize;
    let mut local_xor = 0u64;
    let mut local_sum = 0u64;
    let mut local_entities = 0usize;
    let mut local_has_true = 0usize;

    // SAFETY: `sub_iter` is an active worker `BamIter`.
    while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
        let prev_chunks = acc.chunk_count.fetch_add(1, Ordering::Relaxed);
        if acc.fail_on_chunk_threshold > 0 && prev_chunks + 1 >= acc.fail_on_chunk_threshold {
            return FfiResult::INTERNAL_ERROR;
        }
        // SAFETY: Active chunk of length `len`.
        let ents_ptr = unsafe { host_iter_entities(sub_iter.cast_const()) };
        // SAFETY: Active chunk of length `len`; term 0 is dense `LocalOffset`.
        let off_ptr = unsafe { host_iter_column_readonly(sub_iter.cast_const(), 0) };
        // SAFETY: Active chunk of length `len`; term 1 is `OPTIONAL_READ` `OptBonus`.
        let opt_ptr = unsafe { host_iter_column_readonly(sub_iter.cast_const(), 1) };
        // SAFETY: Active chunk of length `len`; term 2 is `HAS` `TagA`.
        let has_ptr = unsafe { host_iter_column_readonly(sub_iter.cast_const(), 2) };
        if ents_ptr.is_null() || off_ptr.is_null() || has_ptr.is_null() {
            return FfiResult::INTERNAL_ERROR;
        }
        // SAFETY: Slices have exact length `len` matching the active chunk.
        let ents = unsafe { core::slice::from_raw_parts(ents_ptr, len) };
        // SAFETY: `off_ptr` points to `len` contiguous aligned `LocalOffset` elements.
        #[allow(clippy::cast_ptr_alignment)]
        let offs = unsafe { core::slice::from_raw_parts(off_ptr.cast::<LocalOffset>(), len) };
        // SAFETY: `has_ptr` points to `len` contiguous `FfiBool` elements.
        let has_slice = unsafe { core::slice::from_raw_parts(has_ptr.cast::<FfiBool>(), len) };
        let opt_slice = if opt_ptr.is_null() {
            None
        } else {
            // SAFETY: Non-null `opt_ptr` points to `len` contiguous aligned `OptBonus` elements.
            #[allow(clippy::cast_ptr_alignment)]
            Some(unsafe { core::slice::from_raw_parts(opt_ptr.cast::<OptBonus>(), len) })
        };

        for i in 0..len {
            let bonus = opt_slice.map_or(0, |s| s[i].bonus);
            let has_flag = has_slice[i].get();
            if has_flag {
                local_has_true += 1;
            }
            let h = fold_row_hash(ents[i].to_bits(), offs[i], bonus, has_flag, scale);
            local_xor ^= h;
            local_sum = local_sum.wrapping_add(h);
        }
        local_entities += len;
    }

    if acc.fail_on_chunk_threshold == 0 && local_entities != sub_count {
        return FfiResult::INTERNAL_ERROR;
    }

    acc.entity_count
        .fetch_add(local_entities, Ordering::Relaxed);
    acc.has_true_count
        .fetch_add(local_has_true, Ordering::Relaxed);
    acc.xor_hash.fetch_xor(local_xor, Ordering::Relaxed);
    acc.sum_hash.fetch_add(local_sum, Ordering::Relaxed);
    FfiResult::OK
}

#[test]
fn par_readonly_multicore_bit_exact_parity_permissions_errors_and_zero_arena_allocs() {
    // Warm up Bevy's global `ComputeTaskPool` before measuring worker allocations.
    let _pool = bevy_tasks::ComputeTaskPool::get_or_init(bevy_tasks::TaskPool::new);

    let mut world = World::new();
    world.insert_resource(ScaleRes { factor: 17 });
    let off_cid = world.register_component::<LocalOffset>();
    let opt_cid = world.register_component::<OptBonus>();
    let tag_cid = world.register_component::<TagA>();
    let sparse_cid = world.register_component::<SparseWeight>();
    let scale_cid = world.components().component_id::<ScaleRes>().unwrap();

    // Spawn 20,000 entities spread across 4 distinct dense archetypes.
    for i in 0..20_000i64 {
        let off = LocalOffset {
            dx: i,
            dy: -i * 3,
            dz: (i % 97) * 5,
        };
        match i % 4 {
            0 => {
                world.spawn(off);
            }
            1 => {
                world.spawn((off, OptBonus { bonus: i * 11 }));
            }
            2 => {
                world.spawn((off, TagA));
            }
            _ => {
                world.spawn((off, OptBonus { bonus: i * 13 }, TagA));
            }
        }
    }
    // Also spawn 2,000 entities with a `SparseSet` component for archetypal read-only parity.
    let mut expected_sparse_sum = 0i64;
    for i in 1..=2_000i64 {
        world.spawn(SparseWeight { w: i * 7 });
        expected_sparse_sum += i * 7;
    }

    let mut matched_tables = Vec::new();
    for arch in world.archetypes().iter() {
        if arch.contains(off_cid) && !arch.is_empty() {
            let tid = arch.table_id();
            if !matched_tables.contains(&tid) {
                matched_tables.push(tid);
            }
        }
    }
    assert_eq!(matched_tables.len(), 4);

    let ro_plan = QueryPlan::from_terms(vec![
        QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: off_cid,
            mode: AccessMode::READ,
            uuid: LocalOffset::UUID,
            size: core::mem::size_of::<LocalOffset>(),
            align: core::mem::align_of::<LocalOffset>(),
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(2),
            component_id: opt_cid,
            mode: AccessMode::OPTIONAL_READ,
            uuid: OptBonus::UUID,
            size: core::mem::size_of::<OptBonus>(),
            align: core::mem::align_of::<OptBonus>(),
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(3),
            component_id: tag_cid,
            mode: AccessMode::HAS,
            uuid: TagA::UUID,
            size: 0,
            align: 1,
            storage_flags: 0,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
        QueryTermMeta {
            component: BamComponent::from_raw(4),
            component_id: scale_cid,
            mode: AccessMode::RESOURCE_READ,
            uuid: ScaleRes::UUID,
            size: core::mem::size_of::<ScaleRes>(),
            align: core::mem::align_of::<ScaleRes>(),
            storage_flags: BAM_STORAGE_FLAG_RESOURCE,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        },
    ]);

    // 1. Sequential baseline over all 20,000 entities across 4 archetypes.
    let mut seq_iter = HostIter::new_dense(
        &world.storages().tables,
        &matched_tables,
        &ro_plan,
        Tick::new(0),
        Tick::new(1),
    )
    .with_world(world.as_unsafe_world_cell_readonly());

    let mut seq_entities = 0usize;
    let mut seq_has_true = 0usize;
    let mut seq_xor = 0u64;
    let mut seq_sum = 0u64;
    let mut len = 0usize;
    // SAFETY: `seq_iter` is valid on stack and all column/resource pointers match component alignments.
    #[allow(clippy::cast_ptr_alignment)]
    unsafe {
        let scale =
            (*host_res_ptr(seq_iter.as_ffi_ptr().cast_const(), 3).cast::<ScaleRes>()).factor;
        while host_iter_next_chunk(seq_iter.as_ffi_ptr(), &raw mut len).get() {
            let ents = core::slice::from_raw_parts(
                host_iter_entities(seq_iter.as_ffi_ptr().cast_const()),
                len,
            );
            let offs = core::slice::from_raw_parts(
                host_iter_column_readonly(seq_iter.as_ffi_ptr().cast_const(), 0)
                    .cast::<LocalOffset>(),
                len,
            );
            let opt_p = host_iter_column_readonly(seq_iter.as_ffi_ptr().cast_const(), 1);
            let opt_s = if opt_p.is_null() {
                None
            } else {
                Some(core::slice::from_raw_parts(opt_p.cast::<OptBonus>(), len))
            };
            let has_s = core::slice::from_raw_parts(
                host_iter_column_readonly(seq_iter.as_ffi_ptr().cast_const(), 2).cast::<FfiBool>(),
                len,
            );
            for i in 0..len {
                let bonus = opt_s.map_or(0, |s| s[i].bonus);
                let flag = has_s[i].get();
                if flag {
                    seq_has_true += 1;
                }
                let h = fold_row_hash(ents[i].to_bits(), offs[i], bonus, flag, scale);
                seq_xor ^= h;
                seq_sum = seq_sum.wrapping_add(h);
            }
            seq_entities += len;
        }
    }
    assert_eq!(seq_entities, 20_000);
    assert_eq!(seq_has_true, 10_000);

    // 2. Multi-core `host_par_for_each_chunk_readonly` with zero worker heap/command-arena allocations!
    WORKER_ALLOC_COUNT.store(0, Ordering::SeqCst);
    WORKER_ALLOC_BYTES.store(0, Ordering::SeqCst);
    let par_acc = ParParityAcc {
        entity_count: AtomicUsize::new(0),
        chunk_count: AtomicUsize::new(0),
        xor_hash: AtomicU64::new(0),
        sum_hash: AtomicU64::new(0),
        has_true_count: AtomicUsize::new(0),
        nested_par_ro_rc: AtomicUsize::new(0),
        nested_par_rc: AtomicUsize::new(0),
        track_allocs: true,
        fail_on_chunk_threshold: 0,
    };
    let mut par_iter = HostIter::new_dense(
        &world.storages().tables,
        &matched_tables,
        &ro_plan,
        Tick::new(0),
        Tick::new(1),
    )
    .with_world(world.as_unsafe_world_cell_readonly());

    // SAFETY: `par_iter` and `par_acc` remain live across the synchronous `ComputeTaskPool::scope`.
    let par_rc = unsafe {
        host_par_for_each_chunk_readonly(
            par_iter.as_ffi_ptr(),
            Some(dense_parity_worker),
            (&raw const par_acc).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(par_rc, FfiResult::OK);
    assert_eq!(par_acc.entity_count.load(Ordering::Relaxed), seq_entities);
    assert_eq!(par_acc.has_true_count.load(Ordering::Relaxed), seq_has_true);
    assert_eq!(par_acc.xor_hash.load(Ordering::Relaxed), seq_xor);
    assert_eq!(par_acc.sum_hash.load(Ordering::Relaxed), seq_sum);
    assert_eq!(par_iter.stats().1, 20_000);
    assert_eq!(
        par_acc.nested_par_ro_rc.load(Ordering::Relaxed),
        FfiResult::ACCESS_CONFLICT.0 as usize
    );
    assert_eq!(
        par_acc.nested_par_rc.load(Ordering::Relaxed),
        FfiResult::ACCESS_CONFLICT.0 as usize
    );
    // Strict zero-arena & zero per-worker heap allocation guarantee!
    assert_eq!(
        WORKER_ALLOC_COUNT.load(Ordering::SeqCst),
        0,
        "expected ZERO heap/command-arena allocations inside read-only worker tasks"
    );
    assert_eq!(WORKER_ALLOC_BYTES.load(Ordering::SeqCst), 0);

    // 3. Error propagation across multi-core workers returns first non-OK error deterministically.
    par_iter.reset();
    let err_acc = ParParityAcc {
        entity_count: AtomicUsize::new(0),
        chunk_count: AtomicUsize::new(0),
        xor_hash: AtomicU64::new(0),
        sum_hash: AtomicU64::new(0),
        has_true_count: AtomicUsize::new(0),
        nested_par_ro_rc: AtomicUsize::new(0),
        nested_par_rc: AtomicUsize::new(0),
        track_allocs: false,
        fail_on_chunk_threshold: 2,
    };
    // SAFETY: `par_iter` and `err_acc` are live on stack.
    let err_rc = unsafe {
        host_par_for_each_chunk_readonly(
            par_iter.as_ffi_ptr(),
            Some(dense_parity_worker),
            (&raw const err_acc).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(err_rc, FfiResult::INTERNAL_ERROR);

    // 4. Archetypal / `SparseSet` multi-core read-only parity.
    let sparse_aids: Vec<_> = world
        .archetypes()
        .iter()
        .filter(|a| a.contains(sparse_cid) && !a.is_empty())
        .map(|a| a.id())
        .collect();
    let sparse_plan = QueryPlan::from_terms(vec![QueryTermMeta {
        component: BamComponent::from_raw(5),
        component_id: sparse_cid,
        mode: AccessMode::READ,
        uuid: SparseWeight::UUID,
        size: core::mem::size_of::<SparseWeight>(),
        align: core::mem::align_of::<SparseWeight>(),
        storage_flags: BAM_STORAGE_FLAG_SPARSE_SET,
        msg_read_batch_fn: None,
        msg_send_fn: None,
        msg_write_batch_fn: None,
    }]);
    struct SparseSum {
        sum: AtomicU64,
        count: AtomicUsize,
    }
    unsafe extern "C" fn sparse_worker(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to live `SparseSum`.
        let acc = unsafe { &*ud.cast::<SparseSum>() };
        let mut chunk_len = 0usize;
        // SAFETY: `sub_iter` is an active archetypal worker `BamIter`.
        while unsafe { host_iter_next_chunk(sub_iter, &raw mut chunk_len) }.get() {
            // SAFETY: Term 0 is sparse `SparseWeight`.
            let rows = unsafe { host_iter_column_rows_readonly(sub_iter, 0) };
            if rows.is_null() {
                return FfiResult::INTERNAL_ERROR;
            }
            // SAFETY: `rows` holds `chunk_len` valid pointers to `SparseWeight`.
            let slice = unsafe { core::slice::from_raw_parts(rows, chunk_len) };
            let mut local = 0u64;
            for &p in slice {
                // SAFETY: Each pointer targets a live, properly aligned `SparseWeight`.
                #[allow(clippy::cast_ptr_alignment)]
                let w = unsafe { (*p.cast::<SparseWeight>()).w as u64 };
                local = local.wrapping_add(w);
            }
            acc.sum.fetch_add(local, Ordering::Relaxed);
            acc.count.fetch_add(chunk_len, Ordering::Relaxed);
        }
        FfiResult::OK
    }
    let mut scratch = SparseRowScratch::default();
    let mut sparse_iter = HostIter::new_archetypal(
        &world.storages().tables,
        world.archetypes(),
        &world.storages().sparse_sets,
        &sparse_aids,
        &sparse_plan,
        &mut scratch,
        Tick::new(0),
        Tick::new(1),
    );
    let sparse_acc = SparseSum {
        sum: AtomicU64::new(0),
        count: AtomicUsize::new(0),
    };
    // SAFETY: `sparse_iter` and `sparse_acc` are live on stack.
    let sp_rc = unsafe {
        host_par_for_each_chunk_readonly(
            sparse_iter.as_ffi_ptr(),
            Some(sparse_worker),
            (&raw const sparse_acc).cast_mut().cast::<c_void>(),
        )
    };
    assert_eq!(sp_rc, FfiResult::OK);
    assert_eq!(sparse_acc.count.load(Ordering::Relaxed), 2_000);
    assert_eq!(
        sparse_acc.sum.load(Ordering::Relaxed) as i64,
        expected_sparse_sum
    );

    // 5. Reject ALL writable access modes with `PERMISSION_DENIED`.
    for (mode, flags) in [
        (AccessMode::WRITE, 0),
        (AccessMode::OPTIONAL_WRITE, 0),
        (AccessMode::RESOURCE_WRITE, BAM_STORAGE_FLAG_RESOURCE),
        (AccessMode::MESSAGE_WRITE, BAM_STORAGE_FLAG_MESSAGE),
    ] {
        let bad_plan = QueryPlan::from_terms(vec![QueryTermMeta {
            component: BamComponent::from_raw(1),
            component_id: off_cid,
            mode,
            uuid: LocalOffset::UUID,
            size: core::mem::size_of::<LocalOffset>(),
            align: core::mem::align_of::<LocalOffset>(),
            storage_flags: flags,
            msg_read_batch_fn: None,
            msg_send_fn: None,
            msg_write_batch_fn: None,
        }]);
        let mut bad_iter = HostIter::new_dense(
            &world.storages().tables,
            &matched_tables,
            &bad_plan,
            Tick::new(0),
            Tick::new(1),
        );
        // SAFETY: Valid stack `HostIter` and callback.
        let deny_rc = unsafe {
            host_par_for_each_chunk_readonly(bad_iter.as_ffi_ptr(), Some(noop_task), null_mut())
        };
        assert_eq!(
            deny_rc,
            FfiResult::PERMISSION_DENIED,
            "mutable mode {mode:?} must be rejected"
        );
    }
}

// ============================================================================
// 4. End-to-End `HOST_API` Table Dispatch inside Scheduled `GuestSystem`
// ============================================================================

#[test]
fn guest_system_up_cascade_and_par_readonly_via_host_api_table() {
    let mut world = World::new();
    let theme_cid = world.register_component::<ThemeColor>();
    let off_cid = world.register_component::<LocalOffset>();
    let child_of_cid = world.register_component::<ChildOf>();

    let mut reg = ModRegistry::new();
    let theme_h = reg
        .register_component::<ThemeColor>(theme_cid, ComponentOptions::default())
        .unwrap();
    let off_h = reg
        .register_component::<LocalOffset>(off_cid, ComponentOptions::default())
        .unwrap();
    let _child_of_h = reg.register_hierarchy(child_of_cid, true).unwrap();
    world.insert_resource(reg.clone());

    let root = world
        .spawn((
            ThemeColor { rgba: 0xDEAD_BEEF },
            LocalOffset {
                dx: 1,
                dy: 2,
                dz: 3,
            },
        ))
        .id();
    for i in 0..1_024i64 {
        let mid = world.spawn(ChildOf(root)).id();
        world.spawn((
            LocalOffset {
                dx: i,
                dy: i,
                dz: i,
            },
            ChildOf(mid),
        ));
    }

    struct E2eState {
        found_ancestors: AtomicUsize,
        not_found_roots: AtomicUsize,
    }

    unsafe extern "C" fn e2e_worker(sub_iter: *mut BamIter, ud: *mut c_void) -> FfiResult {
        // SAFETY: `ud` points to live `E2eState`.
        let st = unsafe { &*ud.cast::<E2eState>() };
        let up_cascade_fn = HOST_API.iter_up_cascade.expect("iter_up_cascade slot");
        let mut len = 0usize;
        // SAFETY: `sub_iter` is an active read-only worker iterator.
        while unsafe { host_iter_next_chunk(sub_iter, &raw mut len) }.get() {
            // SAFETY: Active chunk of length `len`.
            let ents_ptr = unsafe { host_iter_entities(sub_iter.cast_const()) };
            // SAFETY: `ents_ptr` has `len` elements.
            let ents = unsafe { core::slice::from_raw_parts(ents_ptr, len) };
            for &ent in ents {
                let mut ptr: *const c_void = null();
                let mut owner = FfiEntity::NULL;
                // SAFETY: Calling ABI 0.3 `iter_up_cascade` from inside read-only parallel worker!
                let rc = unsafe {
                    up_cascade_fn(
                        sub_iter,
                        BamComponent::INVALID,
                        ent,
                        1,
                        &raw mut ptr,
                        &raw mut owner,
                    )
                };
                if rc == FfiResult::OK {
                    // SAFETY: `ptr` points to ancestor `ThemeColor`.
                    let rgba = unsafe { (*ptr.cast::<ThemeColor>()).rgba };
                    if rgba != 0xDEAD_BEEF || owner.is_null() {
                        return FfiResult::INTERNAL_ERROR;
                    }
                    st.found_ancestors.fetch_add(1, Ordering::Relaxed);
                } else if rc == FfiResult::NOT_FOUND {
                    if !ptr.is_null() || !owner.is_null() {
                        return FfiResult::INTERNAL_ERROR;
                    }
                    st.not_found_roots.fetch_add(1, Ordering::Relaxed);
                } else {
                    return rc;
                }
            }
        }
        FfiResult::OK
    }

    unsafe extern "C" fn e2e_system(sys_ctx: *mut BamSystemCtx, ud: *mut c_void) -> FfiResult {
        let mut iter: *mut BamIter = null_mut();
        // SAFETY: `sys_ctx` is active during `GuestSystem` execution.
        let rc = unsafe { host_sys_query(sys_ctx, 0, &raw mut iter) };
        if rc.is_err() {
            return rc;
        }
        let par_ro_fn = HOST_API
            .par_for_each_chunk_readonly
            .expect("par_for_each_chunk_readonly slot");
        // SAFETY: `iter` is a strictly read-only query iterator.
        unsafe { par_ro_fn(iter, Some(e2e_worker), ud) }
    }

    let state = Box::new(E2eState {
        found_ancestors: AtomicUsize::new(0),
        not_found_roots: AtomicUsize::new(0),
    });

    let sys = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &raw mut world,
            Some(ModId(42)),
        );
        let terms = [
            BamTermDesc::read(off_h),
            BamTermDesc::optional_read(theme_h),
        ];
        let queries = [BamQueryDesc::new(terms.as_ptr(), terms.len())];
        let desc = BamSystemDesc::new(
            c"v03_cp::e2e_par_up_cascade".as_ptr(),
            queries.as_ptr(),
            queries.len(),
            e2e_system,
            (&*state as *const E2eState).cast_mut().cast::<c_void>(),
        );
        // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
        let reg_rc = unsafe { host_register_system(ctx.as_ffi_ptr(), &desc) };
        assert_eq!(reg_rc, FfiResult::OK);
        ctx.finish_init();
        ctx.registered_systems.pop().unwrap()
    };

    let mut sched = Schedule::default();
    sched.add_systems(sys);
    sched.run(&mut world);

    // 1,024 leaf entities found `root`'s `ThemeColor`, and `root` itself got `NOT_FOUND`.
    assert_eq!(state.found_ancestors.load(Ordering::Relaxed), 1_024);
    assert_eq!(state.not_found_roots.load(Ordering::Relaxed), 1);
}
