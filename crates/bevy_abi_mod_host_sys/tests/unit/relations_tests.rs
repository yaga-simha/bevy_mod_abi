use super::*;
use crate::ffi::commands::{HostCommands, HostCommandsStorage, host_cmd_despawn, host_cmd_remove};
use crate::ffi::register::LoadContext;
use crate::ffi::v03_ext::{
    host_cmd_checkpoint, host_cmd_insert_relation, host_cmd_remove_relation, host_cmd_rollback,
    host_register_relation,
};
use crate::loader::ModId;
use bevy_abi_mod_sys::{
    BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE, BAM_RELATION_FLAG_CASCADE_DESPAWN,
    BAM_RELATION_FLAG_CLEANUP_REMOVE, BAM_RELATION_FLAG_SYMMETRIC, ComponentUuid,
};
use core::ffi::c_char;
use core::ptr::{null, null_mut};
use core::sync::atomic::AtomicBool;
use std::ffi::CString;
use std::sync::Arc;

fn register_rel(
    world: &mut World,
    registry: &mut ModRegistry,
    name: &str,
    flags: u32,
    data_size: usize,
    data_align: usize,
    max_degree: u32,
    mod_id: Option<ModId>,
    manifest_name: Option<&str>,
) -> Result<BamComponent, FfiResult> {
    let cname = CString::new(name).unwrap();
    let full = if !name.contains("::") {
        if let Some(ns) = manifest_name {
            format!("{ns}::{name}")
        } else {
            name.to_string()
        }
    } else {
        name.to_string()
    };
    let mut desc = BamRelationDesc::new(cname.as_ptr(), ComponentUuid::from_canonical_name(&full))
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
    // SAFETY: `load_ctx`, `desc`, and `out` are valid stack-allocated aligned values.
    let status = unsafe { host_register_relation(load_ctx.as_ffi_ptr(), &desc, &raw mut out) };
    *registry = load_ctx.registry.clone();
    load_ctx.finish_init();
    if status.is_ok() { Ok(out) } else { Err(status) }
}

#[test]
fn registration_validation_idempotency_and_standalone_mode() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();

    // Contradictory EXCLUSIVE + max_degree > 1 rejected.
    assert_eq!(
        register_rel(
            &mut world,
            &mut registry,
            "rel::BadExclusive",
            BAM_RELATION_FLAG_EXCLUSIVE,
            0,
            1,
            2,
            None,
            None
        ),
        Err(FfiResult::INVALID_ARGUMENT)
    );

    // Namespace prefixing & idempotent re-registration across mod unload/reload.
    let h1 = register_rel(
        &mut world,
        &mut registry,
        "Follows",
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE,
        8,
        8,
        1,
        Some(ModId(1)),
        Some("ai_mod"),
    )
    .expect("register ai_mod::Follows");
    let meta = registry.get_relation(h1).expect("relation meta");
    assert_eq!(meta.name.as_ref(), "ai_mod::Follows");
    assert!(meta.is_exclusive());
    assert!(meta.is_cascade_despawn());
    assert!(!meta.is_symmetric());
    assert!(!meta.is_acyclic());
    assert!(!meta.is_cleanup_remove());
    assert_eq!(meta.effective_max_degree(), Some(1));
    assert_eq!(registry.relation_count(), 1);
    assert!(registry.is_relation(h1));
    assert!(registry.get_relation_by_id(meta.component_id).is_some());
    assert!(registry.get_relation_by_uuid(&meta.uuid).is_some());

    // Insert edge before reload, evict mod, then re-register identically.
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let payload = 0x1234_5678_9ABC_DEF0u64.to_le_bytes();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h1, a, b, &payload),
        FfiResult::OK
    );
    registry.evict_mod(ModId(1));

    let h2 = register_rel(
        &mut world,
        &mut registry,
        "Follows",
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DESPAWN,
        8,
        8,
        1,
        Some(ModId(2)),
        Some("ai_mod"),
    )
    .expect("idempotent reload");
    assert_eq!(h2, h1);
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_data(h1, a, b),
        Some(payload.as_slice())
    );

    // Mismatched flags / payload / max_degree rejected with TYPE_MISMATCH.
    assert_eq!(
        register_rel(
            &mut world,
            &mut registry,
            "ai_mod::Follows",
            BAM_RELATION_FLAG_SYMMETRIC,
            8,
            8,
            1,
            None,
            None
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        register_rel(
            &mut world,
            &mut registry,
            "ai_mod::Follows",
            BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DESPAWN,
            4,
            4,
            1,
            None,
            None
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );
    assert_eq!(
        register_rel(
            &mut world,
            &mut registry,
            "ai_mod::Follows",
            BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DESPAWN,
            8,
            8,
            0,
            None,
            None
        ),
        Err(FfiResult::TYPE_MISMATCH)
    );

    // Standalone LoadContext::new (null world_ptr) registers cleanly without panicking.
    let mut standalone = LoadContext::new(ModRegistry::new());
    let desc = BamRelationDesc::new(
        c"standalone::Link".as_ptr(),
        ComponentUuid::from_canonical_name("standalone::Link"),
    );
    let mut out = BamComponent::INVALID;
    // SAFETY: Valid standalone `LoadContext` and descriptor.
    unsafe {
        assert_eq!(
            host_register_relation(standalone.as_ffi_ptr(), &desc, &raw mut out),
            FfiResult::OK
        );
        assert!(out.is_valid());
        // Null/bad descriptor header checks.
        let mut short = desc;
        short.struct_size = 4;
        assert_eq!(
            host_register_relation(standalone.as_ffi_ptr(), &short, &raw mut out),
            FfiResult::INVALID_VERSION
        );
        assert_eq!(
            host_register_relation(standalone.as_ffi_ptr(), null(), &raw mut out),
            FfiResult::NULL_POINTER
        );
        assert_eq!(
            host_register_relation(standalone.as_ffi_ptr(), &desc, null_mut()),
            FfiResult::NULL_POINTER
        );
    }
    standalone.finish_init();
}

#[test]
fn exclusive_symmetric_max_degree_and_payload_roundtrip() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();

    // 1. EXCLUSIVE | SYMMETRIC with 4-byte payload.
    let pair_rel = register_rel(
        &mut world,
        &mut registry,
        "test::PairedWith",
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_SYMMETRIC,
        4,
        4,
        0,
        None,
        None,
    )
    .unwrap();
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let d = world.spawn_empty().id();
    let pair_cid = registry.get_relation(pair_rel).unwrap().component_id;

    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, pair_rel, a, b, &10u32.to_le_bytes()),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, pair_rel, c, d, &20u32.to_le_bytes()),
        FfiResult::OK
    );
    // Update payload from reverse direction (B -> A).
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, pair_rel, b, a, &15u32.to_le_bytes()),
        FfiResult::OK
    );
    {
        let g = world.resource::<ModRelationGraph>();
        assert_eq!(
            g.edge_data(pair_rel, a, b),
            Some(15u32.to_le_bytes().as_slice())
        );
        assert_eq!(
            g.edge_data(pair_rel, b, a),
            Some(15u32.to_le_bytes().as_slice())
        );
        assert_eq!(g.edge_count(pair_rel), 4);
    }

    // Re-pair A -> C: replaces BOTH A <-> B and C <-> D atomically and strips markers from B & D.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, pair_rel, a, c, &30u32.to_le_bytes()),
        FfiResult::OK
    );
    {
        let g = world.resource::<ModRelationGraph>();
        assert_eq!(g.targets(pair_rel, a), vec![c]);
        assert_eq!(g.targets(pair_rel, c), vec![a]);
        assert!(g.is_entity_isolated(pair_rel, b));
        assert!(g.is_entity_isolated(pair_rel, d));
        assert_eq!(g.edge_count(pair_rel), 2);
        assert_eq!(
            g.outgoing_edges(pair_rel, a),
            vec![RelationEdgeView {
                target: c,
                data: &30u32.to_le_bytes()
            }]
        );
    }
    assert!(!world.entity(b).contains_id(pair_cid));
    assert!(!world.entity(d).contains_id(pair_cid));
    assert!(world.entity(a).contains_id(pair_cid));
    assert!(world.entity(c).contains_id(pair_cid));

    // Remove from reverse direction (C -> A) strips both directions and markers.
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, pair_rel, c, a),
        FfiResult::OK
    );
    assert!(!world.entity(a).contains_id(pair_cid));
    assert!(!world.entity(c).contains_id(pair_cid));

    // 2. max_degree = 2 with SYMMETRIC enforces cap on both endpoints.
    let cap_rel = register_rel(
        &mut world,
        &mut registry,
        "test::AllyCap2",
        BAM_RELATION_FLAG_SYMMETRIC,
        0,
        1,
        2,
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        registry
            .get_relation(cap_rel)
            .unwrap()
            .effective_max_degree(),
        Some(2)
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cap_rel, a, b, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cap_rel, a, c, &[]),
        FfiResult::OK
    );
    // `a` already has degree 2 -> `a -> d` rejected; `d -> a` also rejected because target `a` has degree 2.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cap_rel, a, d, &[]),
        FfiResult::CAPACITY_EXCEEDED
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cap_rel, d, a, &[]),
        FfiResult::CAPACITY_EXCEEDED
    );
    // Wrong payload size & invalid component / entity checks.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cap_rel, b, d, &[1, 2]),
        FfiResult::INVALID_ARGUMENT
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, BamComponent::from_raw(999), a, b, &[]),
        FfiResult::INVALID_COMPONENT
    );
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, BamComponent::from_raw(999), a, b),
        FfiResult::INVALID_COMPONENT
    );
}

#[test]
fn acyclic_eager_deferred_checkpoint_rollback_and_flush_policies() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let dag_rel = register_rel(
        &mut world,
        &mut registry,
        "test::DependsOn",
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
    let fa = FfiEntity::from_bits(a.to_bits());
    let fb = FfiEntity::from_bits(b.to_bits());
    let fc = FfiEntity::from_bits(c.to_bits());
    let fd = FfiEntity::from_bits(d.to_bits());

    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let cp_ptr = cmds.as_ffi_ptr();
        let mut cp = 0usize;
        // SAFETY: Valid `cmds` and stack output.
        unsafe {
            // Self-loop eagerly rejected.
            assert_eq!(
                host_cmd_insert_relation(cp_ptr, fa, dag_rel, fa, null(), 0),
                FfiResult::CYCLE_DETECTED
            );
            assert_eq!(
                host_cmd_insert_relation(cp_ptr, fa, dag_rel, fb, null(), 0),
                FfiResult::OK
            );
            assert_eq!(host_cmd_checkpoint(cp_ptr, &raw mut cp), FfiResult::OK);
            assert_eq!(
                host_cmd_insert_relation(cp_ptr, fb, dag_rel, fc, null(), 0),
                FfiResult::OK
            );
            // In-queue multi-hop cycle `C -> A` eagerly rejected.
            assert_eq!(
                host_cmd_insert_relation(cp_ptr, fc, dag_rel, fa, null(), 0),
                FfiResult::CYCLE_DETECTED
            );
            // Roll back `B -> C`: now `C -> A` forms valid chain `C -> A -> B`!
            assert_eq!(host_cmd_rollback(cp_ptr, cp), FfiResult::OK);
            assert_eq!(
                host_cmd_insert_relation(cp_ptr, fc, dag_rel, fa, null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.last_relation_flush_status(), FfiResult::OK);
    {
        let g = world.resource::<ModRelationGraph>();
        assert!(g.has_edge(dag_rel, c, a));
        assert!(g.has_edge(dag_rel, a, b));
        assert!(!g.has_edge(dag_rel, b, c));
        assert_eq!(
            check_relation_acyclic(g, dag_rel, b, c),
            FfiResult::CYCLE_DETECTED
        );
        assert_eq!(check_relation_acyclic(g, dag_rel, b, d), FfiResult::OK);
        assert_eq!(
            check_relation_acyclic(g, BamComponent::from_raw(888), a, b),
            FfiResult::INVALID_COMPONENT
        );
    }

    // Cross-flush cycle detection with AbortRemainingRelations policy.
    world
        .resource_mut::<ModRelationGraph>()
        .set_flush_policy(RelationFlushPolicy::AbortRemainingRelations);
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and live entity handles.
        unsafe {
            // `B -> C` closes `C -> A -> B -> C` against live World graph.
            assert_eq!(
                host_cmd_insert_relation(cmds.as_ffi_ptr(), fb, dag_rel, fc, null(), 0),
                FfiResult::OK
            );
            // This valid edge `B -> D` should be skipped due to AbortRemainingRelations.
            assert_eq!(
                host_cmd_insert_relation(cmds.as_ffi_ptr(), fb, dag_rel, fd, null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(
        storage.last_relation_flush_status(),
        FfiResult::CYCLE_DETECTED
    );
    {
        let mut g = world.resource_mut::<ModRelationGraph>();
        assert!(!g.has_edge(dag_rel, b, c));
        assert!(!g.has_edge(dag_rel, b, d));
        assert_eq!(
            g.last_flush_error(),
            Some(&RelationFlushError {
                relation: dag_rel,
                entity: b,
                target: c,
                status: FfiResult::CYCLE_DETECTED,
            })
        );
        let drained = g.take_flush_errors();
        assert_eq!(drained.len(), 1);
        assert!(g.flush_errors().is_empty());
        g.set_flush_policy(RelationFlushPolicy::RecordOnly);
        g.clear_flush_errors();
    }
}

#[test]
fn cascade_despawn_cycle_safe_and_cleanup_remove_hooks() {
    let mut world = World::new();
    let mut registry = ModRegistry::new();

    let cascade_rel = register_rel(
        &mut world,
        &mut registry,
        "test::OwnedBy",
        BAM_RELATION_FLAG_CASCADE_DESPAWN,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let cleanup_rel = register_rel(
        &mut world,
        &mut registry,
        "test::Watches",
        BAM_RELATION_FLAG_CLEANUP_REMOVE | BAM_RELATION_FLAG_SYMMETRIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();

    // 1. Cycle-safe cascade despawn: A -> B -> C -> A, plus bystander D <- A.
    let a = world.spawn_empty().id();
    let b = world.spawn_empty().id();
    let c = world.spawn_empty().id();
    let d = world.spawn_empty().id();
    let cascade_cid = registry.get_relation(cascade_rel).unwrap().component_id;
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_rel, a, b, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_rel, b, c, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_rel, c, a, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cascade_rel, a, d, &[]),
        FfiResult::OK
    );

    // Despawn A -> recursively despawns C and B without looping; bystander D survives cleanly!
    assert!(world.despawn(a));
    assert!(world.get_entity(a).is_err());
    assert!(world.get_entity(b).is_err());
    assert!(world.get_entity(c).is_err());
    assert!(world.get_entity(d).is_ok());
    assert!(!world.entity(d).contains_id(cascade_cid));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cascade_rel),
        0
    );

    // 2. CLEANUP_REMOVE via despawn and via `host_cmd_remove` (on_remove hook).
    let x = world.spawn_empty().id();
    let y = world.spawn_empty().id();
    let z = world.spawn_empty().id();
    let cleanup_cid = registry.get_relation(cleanup_rel).unwrap().component_id;
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cleanup_rel, x, y, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, cleanup_rel, y, z, &[]),
        FfiResult::OK
    );
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .sources(cleanup_rel, y)
            .len(),
        2
    );

    // Despawning Z via cmd_despawn strips Y <-> Z without despawning Y; X <-> Y stays intact.
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and entity handle.
        unsafe {
            assert_eq!(
                host_cmd_despawn(cmds.as_ffi_ptr(), FfiEntity::from_bits(z.to_bits())),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_entity(z).is_err());
    assert!(world.get_entity(y).is_ok());
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(cleanup_rel, x, y)
    );
    assert!(
        !world
            .resource::<ModRelationGraph>()
            .has_edge(cleanup_rel, y, z)
    );

    // Stripping marker component from Y via `host_cmd_remove` triggers `on_remove` hook,
    // cleaning up X <-> Y and stripping marker from X without despawning X or Y!
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and entity handle.
        unsafe {
            assert_eq!(
                host_cmd_remove(
                    cmds.as_ffi_ptr(),
                    FfiEntity::from_bits(y.to_bits()),
                    cleanup_rel
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert!(world.get_entity(x).is_ok());
    assert!(world.get_entity(y).is_ok());
    assert!(!world.entity(x).contains_id(cleanup_cid));
    assert!(!world.entity(y).contains_id(cleanup_cid));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(cleanup_rel),
        0
    );

    // Deferred remove on absent edge is an idempotent OK no-op.
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        // SAFETY: Valid `cmds` and entity handles.
        unsafe {
            assert_eq!(
                host_cmd_remove_relation(
                    cmds.as_ffi_ptr(),
                    FfiEntity::from_bits(x.to_bits()),
                    cleanup_rel,
                    FfiEntity::from_bits(y.to_bits()),
                ),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert_eq!(storage.last_relation_flush_status(), FfiResult::OK);
}

#[test]
fn record_only_policy_symmetric_exclusive_acyclic_queue_and_self_loop_cleanup() {
    let empty = ModRelationGraph::new();
    assert_eq!(empty.relation_count(), 0);
    assert_eq!(empty.flush_policy(), RelationFlushPolicy::LogAndSkip);

    let mut world = World::new();
    let mut registry = ModRegistry::new();
    let pair_acyclic = register_rel(
        &mut world,
        &mut registry,
        "test::ExclusiveSymAcyclic",
        BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_ACYCLIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let plain_rel = register_rel(
        &mut world,
        &mut registry,
        "test::PlainSelf",
        0,
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
    let fa = FfiEntity::from_bits(a.to_bits());
    let fb = FfiEntity::from_bits(b.to_bits());
    let fc = FfiEntity::from_bits(c.to_bits());
    let fd = FfiEntity::from_bits(d.to_bits());

    // Exercise pending EXCLUSIVE | SYMMETRIC replacement + Despawn in `check_pending_acyclic`.
    let snap = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap);
        let p = cmds.as_ffi_ptr();
        // SAFETY: Valid `cmds` and live entity handles.
        unsafe {
            assert_eq!(
                host_cmd_insert_relation(p, fa, pair_acyclic, fb, null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                host_cmd_insert_relation(p, fc, pair_acyclic, fd, null(), 0),
                FfiResult::OK
            );
            // Re-pairing A -> C evicts both A <-> B and C <-> D cleanly under ACYCLIC.
            assert_eq!(
                host_cmd_insert_relation(p, fa, pair_acyclic, fc, null(), 0),
                FfiResult::OK
            );
            // In-queue despawn resets pending adjacency cleanly.
            assert_eq!(host_cmd_despawn(p, fd), FfiResult::OK);
            assert_eq!(
                host_cmd_insert_relation(p, fb, pair_acyclic, fa, null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap);
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(pair_acyclic, b, a)
    );
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(pair_acyclic, a, b)
    );
    assert_eq!(
        check_relation_acyclic(world.resource::<ModRelationGraph>(), pair_acyclic, a, c),
        FfiResult::OK
    );

    // Self-loop despawn cleanup on non-cascade relation.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, plain_rel, c, c, &[]),
        FfiResult::OK
    );
    assert!(world.despawn(c));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(plain_rel),
        0
    );

    // RecordOnly policy records flush errors without warning logs, including remove error path.
    world
        .resource_mut::<ModRelationGraph>()
        .set_flush_policy(RelationFlushPolicy::RecordOnly);
    let (ins_status, ins_abort) =
        apply_insert_relation(&mut world, &snap, a, BamComponent::from_raw(777), b, &[]);
    assert_eq!(ins_status, FfiResult::INVALID_COMPONENT);
    assert!(!ins_abort);
    let (rem_status, rem_abort) =
        apply_remove_relation(&mut world, &snap, a, BamComponent::from_raw(777), b);
    assert_eq!(rem_status, FfiResult::INVALID_COMPONENT);
    assert!(!rem_abort);
    assert_eq!(world.resource::<ModRelationGraph>().flush_errors().len(), 2);
}

#[test]
fn relation_guards_lazy_graph_init_symmetric_pending_remove_and_cleanup_branches() {
    use bevy_abi_mod_sys::BAM_RELATION_KNOWN_FLAGS;

    // 1. Direct table helpers: `is_reachable_skipping(start == goal)`, `remove_all_for_entity`
    // leaving non-empty neighbor lists, `ensure_relation_table` replacement, missing handle hooks.
    let e1 = Entity::from_raw_u32(1).unwrap();
    let e2 = Entity::from_raw_u32(2).unwrap();
    let e3 = Entity::from_raw_u32(3).unwrap();
    let e4 = Entity::from_raw_u32(4).unwrap();

    let mut world = World::new();
    let mut registry = ModRegistry::new();
    // Exercise unqualified name without `manifest_name` (`None`) -> covers line 885 & test helper.
    let plain_no_ns = register_rel(
        &mut world,
        &mut registry,
        "UnqualifiedNoNs",
        BAM_RELATION_KNOWN_FLAGS & !BAM_RELATION_FLAG_EXCLUSIVE,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let meta = registry.get_relation(plain_no_ns).unwrap().clone();
    let mut table = RelationEdgeTable::new(meta.clone());
    assert!(table.is_reachable_skipping(e1, e1, &[]));
    table.insert_directed_raw(e1, e2, Box::new([]));
    table.insert_directed_raw(e3, e2, Box::new([]));
    table.insert_directed_raw(e1, e4, Box::new([]));
    // Removing `e1` leaves `e3 -> e2` (`!in_vec.is_empty()`), and removing `e3` leaves `e1` untouched.
    let iso1 = table.remove_all_for_entity(e1);
    assert_eq!(iso1, vec![e4]);
    assert!(table.has_edge(e3, e2));
    table.insert_directed_raw(e1, e2, Box::new([]));
    let iso2 = table.remove_all_for_entity(e2);
    assert_eq!(iso2, vec![e3, e1]);
    assert!(table.is_isolated(e1));

    let mut graph = ModRelationGraph::new();
    graph.ensure_relation_table(meta.clone());
    let mut alt_meta = meta.clone();
    alt_meta.uuid = ComponentUuid::from_canonical_name("alt::Rel");
    alt_meta.component_id = ComponentId::new(987_654);
    graph.ensure_relation_table(alt_meta);
    assert_eq!(
        graph.on_entity_despawn(BamComponent::from_raw(999), e1),
        (vec![], vec![])
    );
    assert_eq!(
        graph.on_entity_remove(BamComponent::from_raw(999), e1),
        Vec::<Entity>::new()
    );
    assert_eq!(
        check_relation_acyclic(&graph, plain_no_ns, e1, e1),
        FfiResult::CYCLE_DETECTED
    );
    // `check_relation_acyclic` when `(e1 -> e2)` already exists returns `OK`.
    let mut dag_graph = ModRelationGraph::new();
    dag_graph.ensure_relation_table(meta);
    dag_graph
        .tables
        .get_mut(&plain_no_ns)
        .unwrap()
        .insert_directed_raw(e1, e2, Box::new([]));
    assert_eq!(
        check_relation_acyclic(&dag_graph, plain_no_ns, e1, e2),
        FfiResult::OK
    );

    // 2. Lazy `ModRelationGraph` initialization from `ModRegistry` / `RegistrySnapshot` in fresh World.
    let mut lazy_world = World::new();
    let mut lazy_reg = ModRegistry::new();
    let lazy_rel = register_rel(
        &mut lazy_world,
        &mut lazy_reg,
        "lazy::Rel",
        BAM_RELATION_FLAG_ACYCLIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    lazy_world.insert_resource(lazy_reg.clone());
    lazy_world.remove_resource::<ModRelationGraph>();
    let la = lazy_world.spawn_empty().id();
    let lb = lazy_world.spawn_empty().id();
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut lazy_world, lazy_rel, la, lb, &[]),
        FfiResult::OK
    );
    lazy_world.remove_resource::<ModRelationGraph>();
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut lazy_world, lazy_rel, la, lb),
        FfiResult::OK
    );

    let lazy_snap = lazy_reg.snapshot_arc();
    let mut fresh_world_ins = World::new();
    let fa_w = fresh_world_ins.spawn_empty().id();
    let fb_w = fresh_world_ins.spawn_empty().id();
    assert_eq!(
        apply_insert_relation(&mut fresh_world_ins, &lazy_snap, fa_w, lazy_rel, fb_w, &[]),
        (FfiResult::OK, false)
    );
    let mut fresh_world_rem = World::new();
    let fa_r = fresh_world_rem.spawn_empty().id();
    let fb_r = fresh_world_rem.spawn_empty().id();
    assert_eq!(
        apply_remove_relation(&mut fresh_world_rem, &lazy_snap, fa_r, lazy_rel, fb_r),
        (FfiResult::OK, false)
    );
    // `ensure_marker_attached` with unregistered `ComponentId` is a safe no-op.
    ensure_marker_attached(&mut fresh_world_rem, fa_r, ComponentId::new(999_999));

    // 3. `allocate_standalone_component_id` collision loop + `register_relation` error guards.
    let mut standalone_reg = ModRegistry::new();
    let collide_cid = ComponentId::new(1_000_001);
    standalone_reg
        .register_relation_entry(RelationRegSpec {
            comp_id: collide_cid,
            name: "collide::Comp",
            uuid: ComponentUuid::from_canonical_name("collide::Comp"),
            flags: 0,
            data_size: 0,
            data_align: 1,
            max_degree: 0,
            owner: None,
        })
        .unwrap();
    let mut standalone_ctx = LoadContext::new(standalone_reg);
    let rel_ok_desc = BamRelationDesc::new(
        c"sa::Rel".as_ptr(),
        ComponentUuid::from_canonical_name("sa::Rel"),
    );
    let mut rel_out = BamComponent::INVALID;
    let bad_utf8 = [0xFFu8, 0xFE, 0];
    // SAFETY: Valid standalone `LoadContext` and stack descriptors; null/bad pointers test FFI validation.
    unsafe {
        assert_eq!(
            host_register_relation(standalone_ctx.as_ffi_ptr(), &rel_ok_desc, &raw mut rel_out),
            FfiResult::OK
        );
        assert!(rel_out.is_valid());
        assert_eq!(
            host_register_relation(null_mut(), &rel_ok_desc, &raw mut rel_out),
            FfiResult::NULL_POINTER
        );
        let bad_flags = BamRelationDesc::new(
            c"sa::BadFlags".as_ptr(),
            ComponentUuid::from_canonical_name("sa::BadFlags"),
        )
        .with_flags(1 << 31);
        assert_eq!(
            host_register_relation(standalone_ctx.as_ffi_ptr(), &bad_flags, &raw mut rel_out),
            FfiResult::UNSUPPORTED
        );
        let empty_name = BamRelationDesc::new(
            c"".as_ptr(),
            ComponentUuid::from_canonical_name("sa::Empty"),
        );
        assert_eq!(
            host_register_relation(standalone_ctx.as_ffi_ptr(), &empty_name, &raw mut rel_out),
            FfiResult::INVALID_ARGUMENT
        );
        let bad_utf8_desc = BamRelationDesc::new(
            bad_utf8.as_ptr().cast::<c_char>(),
            ComponentUuid::from_canonical_name("sa::BadUtf8"),
        );
        assert_eq!(
            host_register_relation(
                standalone_ctx.as_ffi_ptr(),
                &bad_utf8_desc,
                &raw mut rel_out
            ),
            FfiResult::INVALID_UTF8
        );
        assert_eq!(
            validate_non_empty_cstr(null()),
            Err(FfiResult::NULL_POINTER)
        );
    }
    standalone_ctx.finish_init();

    // 4. `cmd_insert_relation_impl` / `cmd_remove_relation_impl` null/invalid handles + pending SYMMETRIC remove.
    let sym_acyclic = register_rel(
        &mut world,
        &mut registry,
        "test::SymAcyclicRem",
        BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_ACYCLIC,
        0,
        1,
        0,
        None,
        None,
    )
    .unwrap();
    let wa = world.spawn_empty().id();
    let wb = world.spawn_empty().id();
    let fwa = FfiEntity::from_bits(wa.to_bits());
    let fwb = FfiEntity::from_bits(wb.to_bits());
    let snap2 = registry.snapshot_arc();
    let mut storage = HostCommandsStorage::new();
    {
        let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap2);
        let p = cmds.as_ffi_ptr();
        // SAFETY: Valid `cmds` and null/invalid pointer guard tests.
        unsafe {
            assert_eq!(
                cmd_insert_relation_impl(null_mut(), fwa, sym_acyclic, fwb, null(), 0),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                cmd_remove_relation_impl(null_mut(), fwa, sym_acyclic, fwb),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                cmd_insert_relation_impl(p, fwa, sym_acyclic, fwb, null(), 0),
                FfiResult::OK
            );
            assert_eq!(
                cmd_remove_relation_impl(p, fwa, sym_acyclic, fwb),
                FfiResult::OK
            );
            // Re-inserting `wb -> wa` after symmetric pending removal succeeds cleanly.
            assert_eq!(
                cmd_insert_relation_impl(p, fwb, sym_acyclic, fwa, null(), 0),
                FfiResult::OK
            );
        }
    }
    storage.apply_to_world(&mut world, &snap2);
    assert!(
        world
            .resource::<ModRelationGraph>()
            .has_edge(sym_acyclic, wa, wb)
    );
}

#[test]
fn non_acyclic_relations_allow_self_referential_edges_and_clean_up() {
    use bevy_abi_mod_sys::{
        BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_EXCLUSIVE,
        BAM_RELATION_FLAG_SYMMETRIC,
    };
    use std::ffi::CString;

    let mut world = World::new();
    let reg = ModRegistry::new();
    world.insert_resource(reg.clone());

    let mut h_plain = BamComponent::INVALID;
    let mut h_exclusive = BamComponent::INVALID;
    let mut h_symmetric = BamComponent::INVALID;
    let mut h_cascade = BamComponent::INVALID;
    let mut h_acyclic = BamComponent::INVALID;

    let reg = {
        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            std::sync::Arc::new(core::sync::atomic::AtomicBool::new(true)),
            &mut world as *mut World,
            None,
        );
        for (name, flags, data_size, data_align, out) in [
            ("self_rel::Plain", 0, 4, 4, &mut h_plain),
            (
                "self_rel::Exclusive",
                BAM_RELATION_FLAG_EXCLUSIVE,
                4,
                4,
                &mut h_exclusive,
            ),
            (
                "self_rel::Symmetric",
                BAM_RELATION_FLAG_SYMMETRIC,
                0,
                1,
                &mut h_symmetric,
            ),
            (
                "self_rel::Cascade",
                BAM_RELATION_FLAG_CASCADE_DESPAWN,
                0,
                1,
                &mut h_cascade,
            ),
            (
                "self_rel::Acyclic",
                BAM_RELATION_FLAG_ACYCLIC,
                0,
                1,
                &mut h_acyclic,
            ),
        ] {
            let cname = CString::new(name).unwrap();
            let mut desc =
                BamRelationDesc::new(cname.as_ptr(), ComponentUuid::from_canonical_name(name))
                    .with_flags(flags);
            if data_size > 0 {
                desc = desc.with_payload_layout(data_size, data_align);
            }
            // SAFETY: Valid `LoadContext`, `BamRelationDesc`, and output pointer.
            let res = unsafe { host_register_relation(ctx.as_ffi_ptr(), &desc, out) };
            assert_eq!(res, FfiResult::OK);
        }
        let r = ctx.registry.clone();
        ctx.finish_init();
        r
    };

    // Verify `RegisteredRelation::allows_self_referential()` across all flag combinations.
    assert!(reg.get_relation(h_plain).unwrap().allows_self_referential());
    assert!(
        reg.get_relation(h_exclusive)
            .unwrap()
            .allows_self_referential()
    );
    assert!(
        reg.get_relation(h_symmetric)
            .unwrap()
            .allows_self_referential()
    );
    assert!(
        reg.get_relation(h_cascade)
            .unwrap()
            .allows_self_referential()
    );
    assert!(
        !reg.get_relation(h_acyclic)
            .unwrap()
            .allows_self_referential()
    );

    let e1 = world.spawn_empty().id();
    let e2 = world.spawn_empty().id();

    // ACYCLIC strictly rejects `(e1 -> e1)`.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_acyclic, e1, e1, &[]),
        FfiResult::CYCLE_DETECTED
    );

    // 1. Plain non-ACYCLIC self-loop: insert, overwrite payload, marker attach, and remove cleanup.
    let plain_cid = reg.get_relation(h_plain).unwrap().component_id;
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_plain, e1, e1, &7u32.to_ne_bytes()),
        FfiResult::OK
    );
    assert!(world.entity(e1).contains_id(plain_cid));
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .edge_data(h_plain, e1, e1),
        Some(&7u32.to_ne_bytes()[..])
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_plain, e1, e1, &42u32.to_ne_bytes()),
        FfiResult::OK
    );
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .edge_data(h_plain, e1, e1),
        Some(&42u32.to_ne_bytes()[..])
    );
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, h_plain, e1, e1),
        FfiResult::OK
    );
    assert!(!world.entity(e1).contains_id(plain_cid));
    assert!(
        world
            .resource::<ModRelationGraph>()
            .is_entity_isolated(h_plain, e1)
    );

    // 2. EXCLUSIVE self-loop: replaces prior `(e1 -> e2)` cleanly, strips `e2`, and overwrites back.
    let exc_cid = reg.get_relation(h_exclusive).unwrap().component_id;
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            h_exclusive,
            e1,
            e2,
            &1u32.to_ne_bytes()
        ),
        FfiResult::OK
    );
    assert!(world.entity(e2).contains_id(exc_cid));
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(
            &mut world,
            h_exclusive,
            e1,
            e1,
            &2u32.to_ne_bytes()
        ),
        FfiResult::OK
    );
    assert!(!world.entity(e2).contains_id(exc_cid));
    assert_eq!(
        world
            .resource::<ModRelationGraph>()
            .targets(h_exclusive, e1),
        vec![e1]
    );

    // 3. SYMMETRIC self-loop: single `(e1 -> e1)` edge, marker attached, remove isolates cleanly.
    let sym_cid = reg.get_relation(h_symmetric).unwrap().component_id;
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_symmetric, e1, e1, &[]),
        FfiResult::OK
    );
    assert!(world.entity(e1).contains_id(sym_cid));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(h_symmetric),
        1
    );
    assert_eq!(
        ModRelationGraph::remove_edge_in_world(&mut world, h_symmetric, e1, e1),
        FfiResult::OK
    );
    assert!(!world.entity(e1).contains_id(sym_cid));

    // 4. CASCADE_DESPAWN self-loop `(e1 -> e1)` plus dependent `(e2 -> e1)`: despawning `e1`
    //    must terminate BFS cleanly without looping and cascade despawn `e2`.
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_cascade, e1, e1, &[]),
        FfiResult::OK
    );
    assert_eq!(
        ModRelationGraph::insert_edge_in_world(&mut world, h_cascade, e2, e1, &[]),
        FfiResult::OK
    );
    assert!(world.despawn(e1));
    world.flush();
    assert!(!world.entities().contains(e1));
    assert!(!world.entities().contains(e2));
    assert_eq!(
        world.resource::<ModRelationGraph>().edge_count(h_cascade),
        0
    );
}
