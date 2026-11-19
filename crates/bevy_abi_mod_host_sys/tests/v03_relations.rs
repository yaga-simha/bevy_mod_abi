//! Comprehensive integration test suite for ABI `0.3.0` generic entity relationships
//! (`host_register_relation`, `host_cmd_insert_relation`, `host_cmd_remove_relation`,
//! `ModRelationGraph`, `check_relation_acyclic`, and Bevy `0.20` lifecycle hooks).

use bevy_abi_mod_host_sys::registry::catalog::{
    ModRelationGraph, RelationEdgeView, RelationFlushError, RelationFlushPolicy,
    check_relation_acyclic, ensure_relation_graph_in_world,
};
use bevy_abi_mod_host_sys::{
    ComponentOptions, HOST_API, HostCommands, HostCommandsStorage, LoadContext, ModId, ModRegistry,
    host_cmd_checkpoint, host_cmd_despawn, host_cmd_despawn_all, host_cmd_insert,
    host_cmd_insert_relation, host_cmd_remove, host_cmd_remove_relation, host_cmd_rollback,
    host_cmd_spawn_empty, host_component_layout_info, host_register_component,
    host_register_relation, host_register_system, host_resolve_component,
    host_resolve_component_by_uuid,
};
use bevy_abi_mod_macros::ModComponent;
use bevy_abi_mod_sys::{
    AccessMode, BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BamComponent, BamLayoutInfo,
    BamRelationDesc, BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid,
    FfiEntity, FfiResult, ModComponent as _,
};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::schedule::Schedule;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::ptr::null;
use core::sync::atomic::AtomicBool;
use std::ffi::CString;
use std::sync::Arc;

#[repr(C)]
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, ModComponent)]
#[mod_component(name = "rel_test::MarkerTag")]
struct MarkerTag {
    group: u32,
}

#[repr(C, align(16))]
#[derive(Clone, Copy, Debug, PartialEq)]
struct AlignedEdgePayload {
    weight: f64,
    priority: u32,
    flags: u32,
}

fn register_relation_helper(
    world: &mut World,
    registry: &mut ModRegistry,
    name: &str,
    uuid: ComponentUuid,
    flags: u32,
    data_size: usize,
    data_align: usize,
    max_degree: u32,
    mod_id: Option<ModId>,
    manifest_name: Option<&str>,
) -> Result<BamComponent, FfiResult> {
    let cname = CString::new(name).unwrap();
    let mut desc = BamRelationDesc::new(cname.as_ptr(), uuid)
        .with_flags(flags)
        .with_payload_layout(data_size, data_align);
    desc.max_degree = max_degree;
    let mut load_ctx = LoadContext::with_manifest_name(
        registry.clone(),
        Arc::new(AtomicBool::new(true)),
        (&raw mut *world).cast::<World>(),
        mod_id,
        manifest_name.map(Arc::from),
    );
    let mut out = BamComponent::INVALID;
    // SAFETY: `load_ctx`, `desc`, and `out` are valid aligned stack values.
    let status = unsafe { host_register_relation(load_ctx.as_ffi_ptr(), &desc, &raw mut out) };
    *registry = load_ctx.registry.clone();
    load_ctx.finish_init();
    if status.is_ok() { Ok(out) } else { Err(status) }
}

fn ffi_ent(e: Entity) -> FfiEntity {
    FfiEntity::from_bits(e.to_bits())
}

#[test]
fn register_relation_metadata_resolution_collisions_rollback_and_reload() {
    let mut world = World::new();
    let tag_cid = world.register_component::<MarkerTag>();
    let mut registry = ModRegistry::new();
    let tag_h = registry
        .register_component::<MarkerTag>(tag_cid, ComponentOptions::default())
        .unwrap();
    world.insert_resource(registry.clone());

    let rel_uuid = ComponentUuid::from_canonical_name("combat::TargetOf");
    let rel_h = register_relation_helper(
        &mut world,
        &mut registry,
        "TargetOf",
        rel_uuid,
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE,
        8,
        8,
        1,
        Some(ModId(10)),
        Some("combat"),
    )
    .expect("register combat::TargetOf");

    let meta = registry.get_relation(rel_h).expect("RegisteredRelation");
    assert_eq!(meta.name.as_ref(), "combat::TargetOf");
    assert_eq!(meta.uuid, rel_uuid);
    assert_eq!(meta.data_size, 8);
    assert_eq!(meta.data_align, 8);
    assert_eq!(meta.max_degree, 1);
    assert!(meta.is_exclusive());
    assert!(meta.is_cascade_despawn());
    assert_eq!(registry.relation_count(), 1);
    assert_eq!(world.resource::<ModRelationGraph>().relation_count(), 1);

    // Resolve via standard ABI 0.2/0.3 resolution & layout introspection entrypoints.
    {
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(10)),
            Some(Arc::from("combat")),
        );
        let host_ptr = load_ctx.host_ctx.as_ffi_ptr();
        let mut resolved_name = BamComponent::INVALID;
        let mut resolved_uuid = BamComponent::INVALID;
        let mut layout = BamLayoutInfo::empty();
        // SAFETY: Valid `host_ptr`, C string, UUID, and aligned stack output buffers.
        unsafe {
            assert_eq!(
                host_resolve_component(
                    host_ptr,
                    c"TargetOf".as_ptr(),
                    null(),
                    &raw mut resolved_name
                ),
                FfiResult::OK
            );
            assert_eq!(resolved_name, rel_h);
            assert_eq!(
                host_resolve_component_by_uuid(host_ptr, &rel_uuid, &raw mut resolved_uuid),
                FfiResult::OK
            );
            assert_eq!(resolved_uuid, rel_h);
            assert_eq!(
                host_component_layout_info(host_ptr, rel_h, &raw mut layout),
                FfiResult::OK
            );
        }
        assert_eq!(layout.uuid, rel_uuid);
        // Marker component layout in Bevy ECS is zero-sized `(0, 1)`.
        assert_eq!(layout.size, 0);
        assert_eq!(layout.align, 1);

        // Collision: registering a dynamic component with the same name as a relation fails.
        let dup_comp_desc = ComponentDesc::new(c"combat::TargetOf".as_ptr(), rel_uuid, 0, 1);
        let mut dup_out = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx`, descriptor, and output.
        unsafe {
            assert_eq!(
                host_register_component(load_ctx.as_ffi_ptr(), &dup_comp_desc, &raw mut dup_out),
                FfiResult::TYPE_MISMATCH
            );
        }
        load_ctx.finish_init();
    }

    // Collision: registering a relation with an existing component's name or UUID fails.
    assert_eq!(
        register_relation_helper(
            &mut world,
            &mut registry,
            "rel_test::MarkerTag",
            MarkerTag::UUID,
            0,
            0,
            1,
            0,
            None,
            None,
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        register_relation_helper(
            &mut world,
            &mut registry,
            "combat::OtherAliasForSameUuid",
            rel_uuid,
            0,
            0,
            1,
            0,
            None,
            None,
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        register_relation_helper(
            &mut world,
            &mut registry,
            "combat::TargetOf",
            ComponentUuid::from_canonical_name("combat::WrongUuid"),
            BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE,
            8,
            8,
            1,
            None,
            None,
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );

    // Transactional rollback on failed mod init discards uncommitted relation cleanly.
    {
        let mut failing_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            Some(ModId(99)),
            Some(Arc::from("failing")),
        );
        let temp_uuid = ComponentUuid::from_canonical_name("failing::TempRel");
        let temp_desc = BamRelationDesc::new(c"TempRel".as_ptr(), temp_uuid);
        let mut temp_h = BamComponent::INVALID;
        // SAFETY: Valid `failing_ctx`, descriptor, and output.
        unsafe {
            assert_eq!(
                host_register_relation(failing_ctx.as_ffi_ptr(), &temp_desc, &raw mut temp_h),
                FfiResult::OK
            );
        }
        assert!(failing_ctx.registry.is_relation(temp_h));
        failing_ctx.rollback();
        registry = world.resource::<ModRegistry>().clone();
        assert!(!registry.is_relation(temp_h));
        assert!(registry.get_by_name("failing::TempRel").is_none());
    }

    // Hot-reload preservation: create live edges under ModId(10), unload ModId(10), verify hooks
    // still run safely while unloaded, then reload under ModId(11) preserving remaining edges!
    let hunter1 = world.spawn_empty().id();
    let hunter2 = world.spawn_empty().id();
    let prey1 = world.spawn_empty().id();
    let prey2 = world.spawn_empty().id();
    let threat_1 = 100u64.to_le_bytes();
    let threat_2 = 250u64.to_le_bytes();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, rel_h, hunter1, prey1, &threat_1),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, rel_h, hunter2, prey2, &threat_2),
        FfiResult::OK
    );

    // Despawning `prey1` cascade-despawns `hunter1` while preserving `hunter2 -> prey2`.
    assert!(world.despawn(prey1));
    assert!(world.get_entity(hunter1).is_err());
    assert!(world.get_entity(hunter2).is_ok());
    assert!(world.get_entity(prey2).is_ok());

    // Reload under ModId(11): idempotent registration returns same handle & preserves `hunter2 -> prey2`!
    let reloaded_h = register_relation_helper(
        &mut world,
        &mut registry,
        "TargetOf",
        rel_uuid,
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE,
        8,
        8,
        1,
        Some(ModId(11)),
        Some("combat"),
    )
    .expect("idempotent reload");
    assert_eq!(reloaded_h, rel_h);
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .edge_data(rel_h, hunter2, prey2),
        Some(threat_2.as_slice())
    );
    // Ensure non-relation handle still cannot be used for relation ops.
    let _ = tag_h;
}

#[test]
fn aligned_payload_deferred_spawn_and_raw_insert_protection() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let rel_h = register_relation_helper(
        &mut world,
        &mut registry,
        "graph::WeightedLink",
        ComponentUuid::from_canonical_name("graph::WeightedLink"),
        0,
        core::mem::size_of::<AlignedEdgePayload>(),
        core::mem::align_of::<AlignedEdgePayload>(),
        0,
        None,
        None,
    )
    .unwrap();

    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    let mut e1_ffi = FfiEntity::NULL;
    let mut e2_ffi = FfiEntity::NULL;
    let payload_v1 = AlignedEdgePayload {
        weight: 3.5,
        priority: 7,
        flags: 0xAA,
    };
    let payload_v2 = AlignedEdgePayload {
        weight: 9.25,
        priority: 12,
        flags: 0xBB,
    };

    // Spawn two entities and connect them in the same deferred command buffer!
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let ptr = cmds.as_ffi_ptr();
        // SAFETY: Valid `cmds` and stack buffers.
        unsafe {
            assert_eq!(host_cmd_spawn_empty(ptr, &raw mut e1_ffi), FfiResult::OK);
            assert_eq!(host_cmd_spawn_empty(ptr, &raw mut e2_ffi), FfiResult::OK);
            // Attempting raw `host_cmd_insert` directly on a relation handle is denied.
            assert_eq!(
                host_cmd_insert(ptr, e1_ffi, rel_h, null(), 0),
                FfiResult::PERMISSION_DENIED
            );
            // Wrong payload size rejected synchronously.
            assert_eq!(
                host_cmd_insert_relation(
                    ptr,
                    e1_ffi,
                    rel_h,
                    e2_ffi,
                    (&raw const payload_v1).cast::<c_void>(),
                    8,
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_cmd_insert_relation(
                    ptr,
                    e1_ffi,
                    rel_h,
                    e2_ffi,
                    (&raw const payload_v1).cast::<c_void>(),
                    core::mem::size_of::<AlignedEdgePayload>(),
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.last_relation_flush_status(), FfiResult::OK);

    let e1 = Entity::from_bits(e1_ffi.to_bits());
    let e2 = Entity::from_bits(e2_ffi.to_bits());
    {
        let g = world.resource::<ModRelationGraph>();
        let raw = g.edge_data(rel_h, e1, e2).expect("edge payload present");
        assert_eq!(raw.len(), core::mem::size_of::<AlignedEdgePayload>());
        // SAFETY: `raw` has exact size of `AlignedEdgePayload` copied from valid POD bytes.
        let decoded =
            unsafe { core::ptr::read_unaligned(raw.as_ptr().cast::<AlignedEdgePayload>()) };
        assert_eq!(decoded, payload_v1);
        assert_eq!(
            g.outgoing_edges(rel_h, e1),
            vec![RelationEdgeView {
                target: e2,
                data: raw
            }]
        );
    }

    // In-place payload update does not duplicate the edge.
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and payload.
        unsafe {
            assert_eq!(
                host_cmd_insert_relation(
                    cmds.as_ffi_ptr(),
                    e1_ffi,
                    rel_h,
                    e2_ffi,
                    (&raw const payload_v2).cast::<c_void>(),
                    core::mem::size_of::<AlignedEdgePayload>(),
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    {
        let g = world.resource::<ModRelationGraph>();
        assert_eq!(g.edge_count(rel_h), 1);
        let raw = g.edge_data(rel_h, e1, e2).unwrap();
        // SAFETY: Exact size match for `AlignedEdgePayload`.
        let decoded =
            unsafe { core::ptr::read_unaligned(raw.as_ptr().cast::<AlignedEdgePayload>()) };
        assert_eq!(decoded, payload_v2);
    }

    // Despawning target BEFORE a queued `InsertRelation` flushes rejects cleanly with `INVALID_ENTITY`
    // and leaves no orphan marker on the surviving source.
    let e3 = world.spawn_empty().id();
    let e4 = world.spawn_empty().id();
    let rel_cid = registry.get_relation(rel_h).unwrap().component_id;
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and payload.
        unsafe {
            assert_eq!(
                host_cmd_insert_relation(
                    cmds.as_ffi_ptr(),
                    ffi_ent(e3),
                    rel_h,
                    ffi_ent(e4),
                    (&raw const payload_v1).cast::<c_void>(),
                    core::mem::size_of::<AlignedEdgePayload>(),
                ),
                FfiResult::OK
            );
        }
    }
    assert!(world.despawn(e4));
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(
        storage.last_relation_flush_status(),
        FfiResult::INVALID_ENTITY
    );
    assert!(!world.entity(e3).contains_id(rel_cid));
    assert!(!world.resource::<ModRelationGraph>().has_edge(rel_h, e3, e4));
}

#[test]
fn exclusive_max_degree_and_symmetric_invariants() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();

    // 1. `max_degree == 1` without EXCLUSIVE flag behaves as exclusive replacement.
    let deg1_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "test::MountedOn",
        ComponentUuid::from_canonical_name("test::MountedOn"),
        0,
        0,
        1,
        1,
        None,
        None,
    )
    .unwrap();
    assert!(registry.get_relation(deg1_rel).unwrap().is_exclusive());
    let deg1_cid = registry.get_relation(deg1_rel).unwrap().component_id;

    let rider = world.spawn_empty().id();
    let horse1 = world.spawn_empty().id();
    let horse2 = world.spawn_empty().id();

    // Exclusive self-loop -> replace with horse1 -> replace with horse2 -> replace with self-loop -> remove.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, deg1_rel, rider, rider, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, deg1_rel, rider, horse1, &[]),
        FfiResult::OK
    );
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .targets(deg1_rel, rider),
        vec![horse1]
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, deg1_rel, rider, horse2, &[]),
        FfiResult::OK
    );
    assert!(!world.entity(horse1).contains_id(deg1_cid));
    assert!(world.entity(horse2).contains_id(deg1_cid));
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, deg1_rel, rider, rider, &[]),
        FfiResult::OK
    );
    assert!(!world.entity(horse2).contains_id(deg1_cid));
    assert!(world.entity(rider).contains_id(deg1_cid));
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, deg1_rel, rider, rider),
        FfiResult::OK
    );
    assert!(!world.entity(rider).contains_id(deg1_cid));

    // 2. `max_degree == 3` cap with payload updates and post-removal reinsertion.
    let cap3_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "test::BuffSlot",
        ComponentUuid::from_canonical_name("test::BuffSlot"),
        0,
        4,
        4,
        3,
        None,
        None,
    )
    .unwrap();
    let t1 = world.spawn_empty().id();
    let t2 = world.spawn_empty().id();
    let t3 = world.spawn_empty().id();
    let t4 = world.spawn_empty().id();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t1,
            &1u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t2,
            &2u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t3,
            &3u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    // 4th distinct target exceeds cap.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t4,
            &4u32.to_le_bytes()
        ),
        FfiResult::CAPACITY_EXCEEDED
    );
    // Updating existing target t2 succeeds even at full cap.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t2,
            &22u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .edge_data(cap3_rel, rider, t2),
        Some(22u32.to_le_bytes().as_slice())
    );
    // Removing t1 frees a slot for t4.
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, cap3_rel, rider, t1),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            cap3_rel,
            rider,
            t4,
            &4u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .targets(cap3_rel, rider),
        vec![t2, t3, t4]
    );

    // 3. SYMMETRIC self-loop stores a single edge and cleans up cleanly.
    let sym_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "test::AlliedWith",
        ComponentUuid::from_canonical_name("test::AlliedWith"),
        BAM_RELATION_FLAG_SYMMETRIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, sym_rel, t1, t1, &[]),
        FfiResult::OK
    );
    assert_eq!(world.resource::<ModRelationGraph>().edge_count(sym_rel), 1);
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, sym_rel, t1, t1),
        FfiResult::OK
    );
    assert_eq!(world.resource::<ModRelationGraph>().edge_count(sym_rel), 0);
}

#[test]
fn acyclic_dag_diamonds_symmetric_forest_exclusive_reparent_and_rollback() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();

    // 1. Directed DAG with diamond paths.
    let dag_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "dag::Requires",
        ComponentUuid::from_canonical_name("dag::Requires"),
        BAM_RELATION_FLAG_ACYCLIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let d = world.spawn_empty().id();

    // Diamond: A -> B, A -> C, B -> D, C -> D (valid DAG!).
    for (src, dst) in [(a, b), (a, c), (b, d), (c, d)] {
        assert_eq!(
            ModRelationGraph::insert_edge_in_world(&mut world, dag_rel, src, dst, &[]),
            FfiResult::OK
        );
    }
    // Closing any cycle back toward A fails with CYCLE_DETECTED.
    assert_eq!(
        check_relation_acyclic(world.resource::<ModRelationGraph>(), dag_rel, d, a),
        FfiResult::CYCLE_DETECTED
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, dag_rel, d, a, &[]),
        FfiResult::CYCLE_DETECTED
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, dag_rel, d, b, &[]),
        FfiResult::CYCLE_DETECTED
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, dag_rel, d, d, &[]),
        FfiResult::CYCLE_DETECTED
    );
    // Graph invariant untouched (still 4 edges).
    assert_eq!(world.resource::<ModRelationGraph>().edge_count(dag_rel), 4);

    // 2. ACYCLIC | EXCLUSIVE re-parenting without false cycle rejections.
    let tree_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "dag::Parent",
        ComponentUuid::from_canonical_name("dag::Parent"),
        BAM_RELATION_FLAG_ACYCLIC | BAM_RELATION_FLAG_EXCLUSIVE,
        0,
        1,
        1,
        None,
        None,
    )
    .unwrap();
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let p = cmds.as_ffi_ptr();
        // SAFETY: Valid `cmds` and live entity handles.
        unsafe {
            // Queue A -> B -> C, then replace A -> D, so C -> A becomes valid (`B -> C -> A -> D`)!
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(a), tree_rel, ffi_ent(b), null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(b), tree_rel, ffi_ent(c), null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(c), tree_rel, ffi_ent(a), null(), 0),
                FfiResult::CYCLE_DETECTED
            );
            // Re-parent A -> D replaces A -> B in the pending queue!
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(a), tree_rel, ffi_ent(d), null(), 0),
                FfiResult::OK
            );
            // Now C -> A is acyclic!
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(c), tree_rel, ffi_ent(a), null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.last_relation_flush_status(), FfiResult::OK);
    {
        let g = world.resource::<ModRelationGraph>();
        assert_eq!(g.targets(tree_rel, b), vec![c]);
        assert_eq!(g.targets(tree_rel, c), vec![a]);
        assert_eq!(g.targets(tree_rel, a), vec![d]);
        assert_eq!(g.targets(tree_rel, d), Vec::<Entity>::new());
    }

    // 3. In-queue cycle broken by `cmd_remove_relation` and `cmd_despawn` + `cmd_checkpoint`/`rollback`.
    let e1 = world.spawn_empty().id();
    let e2 = world.spawn_empty().id();
    let e3 = world.spawn_empty().id();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let p = cmds.as_ffi_ptr();
        let mut cp = 0usize;
        // SAFETY: Valid `cmds` and live entity handles.
        unsafe {
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e1), dag_rel, ffi_ent(e2), null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e2), dag_rel, ffi_ent(e3), null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e3), dag_rel, ffi_ent(e1), null(), 0),
                FfiResult::CYCLE_DETECTED
            );
            assert_eq!(host_cmd_checkpoint(p, &raw mut cp), FfiResult::OK);
            // Removing `e1 -> e2` breaks the path so `e3 -> e1` becomes valid.
            assert_eq!(
                host_cmd_remove_relation(p, ffi_ent(e1), dag_rel, ffi_ent(e2)),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e3), dag_rel, ffi_ent(e1), null(), 0),
                FfiResult::OK
            );
            // Roll back both the remove and `e3 -> e1`, restoring `e1 -> e2 -> e3`.
            assert_eq!(host_cmd_rollback(p, cp), FfiResult::OK);
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e3), dag_rel, ffi_ent(e1), null(), 0),
                FfiResult::CYCLE_DETECTED
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(dag_rel, e1, e2)
    );
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(dag_rel, e2, e3)
    );
    assert!(
        !world
            .resource::<ModRelationGraph>()
            .has_edge(dag_rel, e3, e1)
    );

    // 4. SYMMETRIC | ACYCLIC undirected forest (tree edges allowed, triangle rejected).
    let forest_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "dag::SpanningTree",
        ComponentUuid::from_canonical_name("dag::SpanningTree"),
        BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_ACYCLIC,
        4,
        4,
        0,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, forest_rel, e1, e2, &1u32.to_le_bytes()),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, forest_rel, e2, e3, &2u32.to_le_bytes()),
        FfiResult::OK
    );
    // Updating existing edge `e1 <-> e2` payload succeeds!
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            forest_rel,
            e1,
            e2,
            &99u32.to_le_bytes()
        ),
        FfiResult::OK
    );
    // Closing triangle `e3 <-> e1` fails with CYCLE_DETECTED!
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, forest_rel, e3, e1, &3u32.to_le_bytes()),
        FfiResult::CYCLE_DETECTED
    );

    // 5. Deferred flush policy LogAndSkip continues applying valid edges after a rejected cycle.
    world
        .resource_mut::<ModRelationGraph>()
        .set_flush_policy(RelationFlushPolicy::LogAndSkip);
    world
        .resource_mut::<ModRelationGraph>()
        .clear_flush_errors();
    let e4 = world.spawn_empty().id();
    let snap2 = registry.snapshot_arc();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap2);
        let p = cmds.as_ffi_ptr();
        // SAFETY: Valid `cmds` and live entities.
        unsafe {
            // Closes cycle `e1 -> e2 -> e3 -> e1` against live World edges.
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e3), dag_rel, ffi_ent(e1), null(), 0),
                FfiResult::OK
            );
            // Valid subsequent edge `e3 -> e4` MUST still flush under `LogAndSkip`.
            assert_eq!(
                host_cmd_insert_relation(p, ffi_ent(e3), dag_rel, ffi_ent(e4), null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap2);
    assert_eq!(
        storage.last_relation_flush_status(),
        FfiResult::CYCLE_DETECTED
    );
    {
        let g = world.resource::<ModRelationGraph>();
        assert!(!g.has_edge(dag_rel, e3, e1));
        assert!(g.has_edge(dag_rel, e3, e4));
        assert_eq!(
            g.flush_errors(),
            &[RelationFlushError {
                relation: dag_rel,
                entity: e3,
                target: e1,
                status: FfiResult::CYCLE_DETECTED,
            }]
        );
    }
}

#[test]
fn cascade_despawn_deep_chain_diamond_cycles_cross_relation_and_despawn_all() {
    let mut world = World::new();
    let tag_cid = world.register_component::<MarkerTag>();
    let mut registry = ModRegistry::new();
    let tag_h = registry
        .register_component::<MarkerTag>(tag_cid, ComponentOptions::default())
        .unwrap();

    let cascade_r1 = register_relation_helper(
        &mut world,
        &mut registry,
        "cascade::OwnedBy",
        ComponentUuid::from_canonical_name("cascade::OwnedBy"),
        BAM_RELATION_FLAG_CASCADE_DELETE,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let cascade_r2 = register_relation_helper(
        &mut world,
        &mut registry,
        "cascade::AttachedTo",
        ComponentUuid::from_canonical_name("cascade::AttachedTo"),
        BAM_RELATION_FLAG_CASCADE_DESPAWN,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();

    // 1. Deep 500-entity chain (proves iterative BFS without stack overflow).
    let mut chain = Vec::with_capacity(500);
    for _ in 0..500 {
        chain.push(world.spawn_empty().id());
    }
    for i in 1..chain.len() {
        assert_eq!(
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                cascade_r1,
                chain[i],
                chain[i - 1],
                &[]
            ),
            FfiResult::OK
        );
    }
    assert!(world.despawn(chain[0]));
    for &e in &chain {
        assert!(
            world.get_entity(e).is_err(),
            "entity {e:?} should have cascaded"
        );
    }
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cascade_r1),
        0
    );

    // 2. Diamond cascade + cross-relation cascade:
    //    `leaf --R2--> mid_left --R1--> root`, `mid_right --R1--> root`, `shared --R1--> mid_left & mid_right`.
    let root = world.spawn(MarkerTag { group: 1 }).id();
    let mid_left = world.spawn_empty().id();
    let mid_right = world.spawn_empty().id();
    let shared = world.spawn_empty().id();
    let leaf = world.spawn_empty().id();
    let bystander = world.spawn(MarkerTag { group: 2 }).id();

    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, mid_left, root, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, mid_right, root, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, shared, mid_left, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, shared, mid_right, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r2, leaf, mid_left, &[]),
        FfiResult::OK
    );
    // Outgoing edge from `mid_right` to `bystander`: despawning `mid_right` MUST NOT despawn `bystander`.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, mid_right, bystander, &[]),
        FfiResult::OK
    );

    // Despawn `root` via `host_cmd_despawn_all` filtering `MarkerTag`? Wait: `bystander` also has
    // `MarkerTag`, so remove `MarkerTag` from `bystander` first and use `cmd_despawn_all` to kill `root`.
    world.entity_mut(bystander).remove::<MarkerTag>();
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let terms = [BamTermDesc::new(tag_h, AccessMode::FILTER_ONLY)];
        // SAFETY: Valid `cmds` and `terms` slice.
        unsafe {
            assert_eq!(
                host_cmd_despawn_all(cmds.as_ffi_ptr(), terms.as_ptr(), terms.len()),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);

    assert!(world.get_entity(root).is_err());
    assert!(world.get_entity(mid_left).is_err());
    assert!(world.get_entity(mid_right).is_err());
    assert!(world.get_entity(shared).is_err());
    assert!(world.get_entity(leaf).is_err());
    assert!(world.get_entity(bystander).is_ok());
    let r1_cid = registry.get_relation(cascade_r1).unwrap().component_id;
    assert!(!world.entity(bystander).contains_id(r1_cid));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cascade_r1),
        0
    );
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cascade_r2),
        0
    );

    // 3. Self-loop under CASCADE_DESPAWN terminates cleanly.
    let solo = world.spawn_empty().id();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_r1, solo, solo, &[]),
        FfiResult::OK
    );
    assert!(world.despawn(solo));
    assert!(world.get_entity(solo).is_err());
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cascade_r1),
        0
    );
}

#[test]
fn cleanup_remove_on_despawn_strip_and_multi_relation_isolation() {
    let mut world = World::new();
    ensure_relation_graph_in_world(&mut world);
    let mut registry = ModRegistry::new();

    let clean_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "clean::TargetLock",
        ComponentUuid::from_canonical_name("clean::TargetLock"),
        BAM_RELATION_FLAG_CLEANUP_REMOVE,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let keep_rel = register_relation_helper(
        &mut world,
        &mut registry,
        "clean::SquadLink",
        ComponentUuid::from_canonical_name("clean::SquadLink"),
        BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_CLEANUP_REMOVE,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();

    let s1 = world.spawn_empty().id();
    let s2 = world.spawn_empty().id();
    let target = world.spawn_empty().id();
    let clean_cid = registry.get_relation(clean_rel).unwrap().component_id;
    let keep_cid = registry.get_relation(keep_rel).unwrap().component_id;

    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, clean_rel, s1, target, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, clean_rel, s2, target, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, keep_rel, s1, s2, &[]),
        FfiResult::OK
    );

    // Despawning `target` strips `clean_rel` edges & markers from `s1` & `s2` without despawning them,
    // and leaves `keep_rel` (`s1 <-> s2`) completely intact!
    assert!(world.despawn(target));
    assert!(world.get_entity(s1).is_ok());
    assert!(world.get_entity(s2).is_ok());
    assert!(!world.entity(s1).contains_id(clean_cid));
    assert!(!world.entity(s2).contains_id(clean_cid));
    assert!(world.entity(s1).contains_id(keep_cid));
    assert!(world.entity(s2).contains_id(keep_cid));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(clean_rel),
        0
    );
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(keep_rel, s1, s2)
    );
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(keep_rel, s2, s1)
    );

    // Direct `remove_by_id` on `s1` fires `on_remove` hook, cleaning up `s1 <-> s2` & stripping `s2`'s marker!
    world.entity_mut(s1).remove_by_id(keep_cid);
    assert!(world.get_entity(s1).is_ok());
    assert!(world.get_entity(s2).is_ok());
    assert!(!world.entity(s1).contains_id(keep_cid));
    assert!(!world.entity(s2).contains_id(keep_cid));
    assert_eq!(world.resource::<ModRelationGraph>().edge_count(keep_rel), 0);
}

#[test]
fn scheduled_guest_system_executes_relation_commands_via_host_api() {
    struct SysState {
        rel: BamComponent,
        a: FfiEntity,
        b: FfiEntity,
        c: FfiEntity,
        phase: u32,
    }

    unsafe extern "C" fn rel_guest_system(
        ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to live `SysState` owned by the test for the schedule run.
        let state = unsafe { &mut *user_data.cast::<SysState>() };
        let mut cmds = core::ptr::null_mut();
        // SAFETY: `HOST_API` slots are always populated and `ctx` is an active `HostSystemCtx`.
        let cmd_res = unsafe { (HOST_API.sys_commands.unwrap())(ctx, &raw mut cmds) };
        if cmd_res.is_err() {
            return cmd_res;
        }
        let insert_fn = HOST_API.cmd_insert_relation.unwrap();
        let remove_fn = HOST_API.cmd_remove_relation.unwrap();
        let weight = 77u32;
        if state.phase == 0 {
            state.phase = 1;
            // SAFETY: Valid `cmds`, entities, and stack `weight`.
            let r1 = unsafe {
                insert_fn(
                    cmds,
                    state.a,
                    state.rel,
                    state.b,
                    (&raw const weight).cast::<c_void>(),
                    4,
                )
            };
            if r1.is_err() {
                return r1;
            }
            // SAFETY: Valid `cmds`, entities, and stack `weight`.
            unsafe {
                insert_fn(
                    cmds,
                    state.b,
                    state.rel,
                    state.c,
                    (&raw const weight).cast::<c_void>(),
                    4,
                )
            }
        } else {
            // SAFETY: Valid `cmds` and entity handles.
            unsafe { remove_fn(cmds, state.a, state.rel, state.b) }
        }
    }

    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let rel_h = register_relation_helper(
        &mut world,
        &mut registry,
        "sched::FlowsTo",
        ComponentUuid::from_canonical_name("sched::FlowsTo"),
        BAM_RELATION_FLAG_ACYCLIC | BAM_RELATION_FLAG_CLEANUP_REMOVE,
        4,
        4,
        0,
        Some(ModId(7)),
        Some("sched"),
    )
    .unwrap();

    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let mut state = Box::new(SysState {
        rel: rel_h,
        a: ffi_ent(a),
        b: ffi_ent(b),
        c: ffi_ent(c),
        phase: 0,
    });

    let active = Arc::new(AtomicBool::new(true));
    let systems = {
        let mut load_ctx = LoadContext::with_manifest_name(
            registry.clone(),
            Arc::clone(&active),
            (&raw mut world).cast::<World>(),
            Some(ModId(7)),
            Some(Arc::from("sched")),
        );
        let desc = BamSystemDesc::new(
            c"sched::rel_sys".as_ptr(),
            null(),
            0,
            rel_guest_system,
            (&raw mut *state).cast::<c_void>(),
        );
        // SAFETY: Valid `load_ctx` and `desc`.
        let status = unsafe { host_register_system(load_ctx.as_ffi_ptr(), &desc) };
        assert_eq!(status, FfiResult::OK);
        let registered = core::mem::take(&mut load_ctx.registered_systems);
        load_ctx.finish_init();
        registered
    };

    let mut schedule = Schedule::default();
    for sys in systems {
        schedule.add_systems(sys);
    }

    // Frame 1: guest system queues `a -> b` and `b -> c` via `HOST_API.cmd_insert_relation`.
    schedule.run(&mut world);
    {
        let g = world.resource::<ModRelationGraph>();
        assert_eq!(
            g.edge_data(rel_h, a, b),
            Some(77u32.to_le_bytes().as_slice())
        );
        assert_eq!(
            g.edge_data(rel_h, b, c),
            Some(77u32.to_le_bytes().as_slice())
        );
        assert_eq!(g.edge_count(rel_h), 2);
    }

    // Frame 2: guest system queues `remove(a, b)` via `HOST_API.cmd_remove_relation`.
    schedule.run(&mut world);
    {
        let g = world.resource::<ModRelationGraph>();
        assert!(!g.has_edge(rel_h, a, b));
        assert!(g.has_edge(rel_h, b, c));
        assert_eq!(g.edge_count(rel_h), 1);
    }

    // Also verify `host_cmd_despawn` & `host_cmd_remove` flush paths cleanly.
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and entity handle.
        unsafe {
            assert_eq!(
                host_cmd_remove(cmds.as_ffi_ptr(), ffi_ent(b), rel_h),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_despawn(cmds.as_ffi_ptr(), ffi_ent(c)),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(world.resource::<ModRelationGraph>().edge_count(rel_h), 0);
}
