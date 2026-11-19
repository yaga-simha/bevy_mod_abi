#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! Fuzz Target: ABI `0.3.0` typed entity relationship lifecycle (`host_register_relation` across
//! `EXCLUSIVE`, `SYMMETRIC`, `ACYCLIC`, `CASCADE_DELETE`, and `CLEANUP_REMOVE`,
//! `host_cmd_insert_relation`, `host_cmd_remove_relation`, deferred + synchronous entity despawns,
//! and `host_cmd_checkpoint` / `host_cmd_rollback`), asserting `ModRelationGraph` invariants
//! (adjacency symmetry, exclusivity, max-degree, acyclicity, zero dangling endpoints, and marker
//! component parity) never panic or break under arbitrary operation sequences.

use bevy_abi_mod_host_sys::registry::catalog::{
    ModRelationGraph, RelationFlushPolicy, ensure_relation_graph_in_world,
};
use bevy_abi_mod_host_sys::{
    HostCommands, HostCommandsStorage, LoadContext, ModRegistry, host_cmd_checkpoint,
    host_cmd_despawn, host_cmd_insert_relation, host_cmd_remove_relation, host_cmd_rollback,
    host_cmd_spawn_empty, host_register_relation,
};
use bevy_abi_mod_sys::{
    BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BamComponent, BamRelationDesc,
    ComponentUuid, FfiEntity, FfiResult,
};
use bevy_ecs::entity::Entity;
use bevy_ecs::world::World;
use core::ffi::c_void;
use core::sync::atomic::AtomicBool;
use libfuzzer_sys::fuzz_target;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::CString;
use std::hint::black_box;
use std::sync::Arc;

const ALL_FLAG_COMBOS: [u32; 8] = [
    BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_ACYCLIC | BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_SYMMETRIC | BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_ACYCLIC | BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_ACYCLIC,
    BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DELETE,
];

const PAYLOAD_LAYOUTS: [(usize, usize); 4] = [(0, 1), (4, 4), (8, 8), (16, 8)];

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn next_u8(&mut self) -> Option<u8> {
        let b = *self.data.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }
}

fn verify_graph_invariants(world: &World, relations: &[BamComponent], tracked_entities: &[Entity]) {
    let Some(graph) = world.get_resource::<ModRelationGraph>() else {
        return;
    };

    for &rel in relations {
        let Some(meta) = graph.relation_meta(rel) else {
            continue;
        };
        let marker_cid = meta.component_id;
        let mut live_nodes: BTreeSet<Entity> = BTreeSet::new();
        let mut adjacency: BTreeMap<Entity, Vec<Entity>> = BTreeMap::new();

        for &entity in tracked_entities {
            let is_alive = world.entities().contains(entity);
            let targets = graph.targets(rel, entity);
            let sources = graph.sources(rel, entity);

            if !is_alive {
                assert!(
                    targets.is_empty(),
                    "despawned entity {entity:?} retained outgoing edges in relation {:?}",
                    rel
                );
                assert!(
                    sources.is_empty(),
                    "despawned entity {entity:?} retained incoming edges in relation {:?}",
                    rel
                );
                continue;
            }

            live_nodes.insert(entity);

            // Marker component parity: entity carries marker iff it has >= 1 incident edge.
            let isolated = graph.is_entity_isolated(rel, entity);
            let has_marker = world
                .get_entity(entity)
                .is_ok_and(|e| e.contains_id(marker_cid));
            assert_eq!(
                has_marker, !isolated,
                "marker parity mismatch for entity {entity:?} on relation {:?}",
                rel
            );

            // Out-degree bounds (`EXCLUSIVE` -> <= 1; non-exclusive `max_degree` -> <= max_degree).
            if meta.is_exclusive() {
                assert!(
                    targets.len() <= 1,
                    "EXCLUSIVE relation {:?} exceeded out-degree 1 on {entity:?}: len={}",
                    rel,
                    targets.len()
                );
            } else if let Some(max_deg) = meta.effective_max_degree() {
                assert!(
                    targets.len() <= max_deg,
                    "relation {:?} exceeded max_degree {max_deg} on {entity:?}: len={}",
                    rel,
                    targets.len()
                );
            }

            // No duplicate outgoing targets; every target is alive and lists `entity` in `sources`.
            let mut seen_targets: BTreeSet<Entity> = BTreeSet::new();
            for &target in &targets {
                assert!(
                    seen_targets.insert(target),
                    "duplicate outgoing edge ({entity:?} -> {target:?}) in relation {:?}",
                    rel
                );
                assert!(
                    world.entities().contains(target),
                    "dangling outgoing edge ({entity:?} -> {target:?}) in relation {:?}",
                    rel
                );
                assert!(
                    graph.sources(rel, target).contains(&entity),
                    "asymmetric outgoing/incoming index for ({entity:?} -> {target:?}) in {:?}",
                    rel
                );

                // Edge payload length invariant.
                let payload = graph
                    .edge_data(rel, entity, target)
                    .expect("existing edge must expose payload slice");
                assert_eq!(payload.len(), meta.data_size);

                // Symmetry invariant: `(u -> v)` implies `(v -> u)` with identical payload bytes.
                if meta.is_symmetric() {
                    assert!(
                        graph.has_edge(rel, target, entity),
                        "SYMMETRIC relation {:?} missing reciprocal edge ({target:?} -> {entity:?})",
                        rel
                    );
                    assert_eq!(
                        graph.edge_data(rel, target, entity),
                        Some(payload),
                        "SYMMETRIC relation {:?} payload mismatch on ({entity:?} <-> {target:?})",
                        rel
                    );
                }

                // Self-loop prohibition on acyclic relations.
                if meta.is_acyclic() {
                    assert_ne!(
                        entity, target,
                        "ACYCLIC relation {:?} allowed self-loop on {entity:?}",
                        rel
                    );
                }
            }

            // Every incoming source is alive and has a corresponding outgoing edge to `entity`.
            for &src in &sources {
                assert!(
                    world.entities().contains(src),
                    "dangling incoming edge ({src:?} -> {entity:?}) in relation {:?}",
                    rel
                );
                assert!(
                    graph.has_edge(rel, src, entity),
                    "orphan incoming edge ({src:?} -> {entity:?}) in relation {:?}",
                    rel
                );
            }

            if !targets.is_empty() {
                adjacency.insert(entity, targets);
            }
        }

        // Full Kahn topological sort verifying directed acyclicity for non-symmetric ACYCLIC graphs.
        if meta.is_acyclic() && !meta.is_symmetric() {
            let mut in_deg: BTreeMap<Entity, usize> =
                live_nodes.iter().copied().map(|e| (e, 0usize)).collect();
            for targets in adjacency.values() {
                for &dst in targets {
                    *in_deg.entry(dst).or_insert(0) += 1;
                }
            }
            let mut queue: VecDeque<Entity> = in_deg
                .iter()
                .filter_map(|(&e, &d)| (d == 0).then_some(e))
                .collect();
            let mut visited = 0usize;
            while let Some(node) = queue.pop_front() {
                visited += 1;
                if let Some(targets) = adjacency.get(&node) {
                    for &dst in targets {
                        if let Some(d) = in_deg.get_mut(&dst) {
                            *d -= 1;
                            if *d == 0 {
                                queue.push_back(dst);
                            }
                        }
                    }
                }
            }
            assert_eq!(
                visited,
                in_deg.len(),
                "ACYCLIC relation {:?} contains a directed cycle (visited {visited} of {})",
                rel,
                in_deg.len()
            );
        }
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() < 4 {
        return;
    }

    let mut cur = Cursor::new(data);
    let mut world = World::new();
    world.insert_resource(ModRegistry::new());
    ensure_relation_graph_in_world(&mut world);

    let policy_sel = cur.next_u8().unwrap_or(0);
    world
        .resource_mut::<ModRelationGraph>()
        .set_flush_policy(if policy_sel & 1 == 0 {
            RelationFlushPolicy::RecordOnly
        } else {
            RelationFlushPolicy::AbortRemainingRelations
        });

    // Register 1..=6 deterministic/fuzzed relations plus 1 adversarial descriptor probe.
    let rel_count = usize::from(cur.next_u8().unwrap_or(1) % 6) + 1;
    let mut registered_relations: Vec<(BamComponent, usize)> = Vec::with_capacity(rel_count);
    {
        let reg = world.resource::<ModRegistry>().clone();
        let mut load_ctx = LoadContext::with_manifest_name(
            reg,
            Arc::new(AtomicBool::new(true)),
            (&raw mut world).cast::<World>(),
            None,
            Some(Arc::from("fuzz_rel")),
        );

        for idx in 0..rel_count {
            let flags_byte = cur.next_u8().unwrap_or(idx as u8);
            let layout_byte = cur.next_u8().unwrap_or(0);
            let deg_byte = cur.next_u8().unwrap_or(0);

            let flags = ALL_FLAG_COMBOS[usize::from(flags_byte) % ALL_FLAG_COMBOS.len()];
            let (data_size, data_align) =
                PAYLOAD_LAYOUTS[usize::from(layout_byte) % PAYLOAD_LAYOUTS.len()];
            let max_degree = if (flags & BAM_RELATION_FLAG_EXCLUSIVE) != 0 {
                u32::from(deg_byte % 2)
            } else {
                u32::from(deg_byte % 5)
            };

            let name = CString::new(format!("fuzz_rel::Rel_{idx}")).expect("valid cstr");
            let uuid = ComponentUuid::from_parts(
                &format!("fuzz_rel::Rel_{idx}"),
                data_size,
                data_align,
                u64::from(flags),
            );
            let mut desc = BamRelationDesc::new(name.as_ptr(), uuid)
                .with_flags(flags)
                .with_payload_layout(data_size, data_align);
            desc.max_degree = max_degree;

            let mut out_h = BamComponent::INVALID;
            // SAFETY: Active `load_ctx`, valid stack `desc` with NUL-terminated `name`, writable `out_h`.
            let rc = unsafe {
                host_register_relation(load_ctx.as_ffi_ptr(), &raw const desc, &raw mut out_h)
            };
            if rc == FfiResult::OK && out_h.is_valid() {
                registered_relations.push((out_h, data_size));
            }
        }

        // Adversarial descriptor probe: arbitrary raw flags/max_degree/alignment/null checks.
        let adv_flags =
            u32::from(cur.next_u8().unwrap_or(0)) | (u32::from(cur.next_u8().unwrap_or(0)) << 24);
        let adv_align = usize::from(cur.next_u8().unwrap_or(3) % 8);
        let adv_size = usize::from(cur.next_u8().unwrap_or(5) % 19);
        let adv_deg = u32::from(cur.next_u8().unwrap_or(2));
        let adv_name = c"fuzz_rel::Adversarial";
        let mut adv_desc = BamRelationDesc::new(
            adv_name.as_ptr(),
            ComponentUuid::from_canonical_name("fuzz_rel::Adversarial"),
        )
        .with_flags(adv_flags)
        .with_payload_layout(adv_size, adv_align);
        adv_desc.max_degree = adv_deg;
        let mut adv_out = BamComponent::INVALID;
        // SAFETY: Valid `load_ctx` and stack descriptor; malformed flag/align combinations are rejected safely.
        let _ = black_box(unsafe {
            host_register_relation(load_ctx.as_ffi_ptr(), &raw const adv_desc, &raw mut adv_out)
        });
        // SAFETY: Null pointers are explicitly rejected by `host_register_relation`.
        let _ = black_box(unsafe {
            host_register_relation(load_ctx.as_ffi_ptr(), core::ptr::null(), &raw mut adv_out)
        });

        load_ctx.finish_init();
    }

    if registered_relations.is_empty() {
        return;
    }

    let rel_handles: Vec<BamComponent> = registered_relations.iter().map(|&(h, _)| h).collect();

    // Seed 2..=10 live entities tracked for invariant verification plus adversarial handles.
    let init_entities = usize::from(cur.next_u8().unwrap_or(4) % 9) + 2;
    let mut tracked_entities: Vec<Entity> = (0..init_entities)
        .map(|_| world.spawn_empty().id())
        .collect();
    let mut ffi_pool: Vec<FfiEntity> = tracked_entities
        .iter()
        .map(|e| FfiEntity::from_bits(e.to_bits()))
        .collect();
    ffi_pool.push(FfiEntity::NULL);
    ffi_pool.push(FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()));
    ffi_pool.push(FfiEntity::from_bits(0xDEAD_BEEF_0000_0007));

    let snapshot = world.resource::<ModRegistry>().snapshot_arc();
    let mut storage = HostCommandsStorage::new();

    // Run up to 6 batch rounds (each round queues deferred ops, checkpoints/rollbacks, then
    // commits or discards, followed by full graph invariant checking).
    for _round in 0..6 {
        if cur.remaining() == 0 {
            break;
        }
        let mut checkpoints: Vec<usize> = Vec::new();
        let mut newly_spawned: Vec<Entity> = Vec::new();
        let mut sync_despawns: Vec<Entity> = Vec::new();

        {
            let allocator = world.entity_allocator();
            let mut cmds = HostCommands::new(&mut storage, allocator, &snapshot);
            let cmds_ptr = cmds.as_ffi_ptr();

            let ops_in_round = usize::from(cur.next_u8().unwrap_or(8) % 24) + 1;
            for _ in 0..ops_in_round {
                let Some(op_byte) = cur.next_u8() else {
                    break;
                };
                match op_byte % 8 {
                    0..=2 => {
                        // `cmd_insert_relation` (both valid sizes and occasional mismatched sizes).
                        let rel_idx =
                            usize::from(cur.next_u8().unwrap_or(0)) % registered_relations.len();
                        let (rel_h, expected_size) = registered_relations[rel_idx];
                        let src =
                            ffi_pool[usize::from(cur.next_u8().unwrap_or(0)) % ffi_pool.len()];
                        let dst =
                            ffi_pool[usize::from(cur.next_u8().unwrap_or(0)) % ffi_pool.len()];
                        let mode = cur.next_u8().unwrap_or(0);
                        let seed_byte = cur.next_u8().unwrap_or(0x5A);

                        let (data_size, use_null) = match mode % 8 {
                            0 => (expected_size.wrapping_add(1).min(24), false),
                            1 => (expected_size, true),
                            _ => (expected_size, false),
                        };
                        let payload = [seed_byte; 24];
                        let ptr = if use_null || data_size == 0 {
                            core::ptr::null()
                        } else {
                            payload.as_ptr().cast::<c_void>()
                        };
                        // SAFETY: Active `cmds_ptr`; `payload` has 24 readable bytes >= `data_size` when non-null.
                        let _ = black_box(unsafe {
                            host_cmd_insert_relation(cmds_ptr, src, rel_h, dst, ptr, data_size)
                        });
                    }
                    3 => {
                        // `cmd_remove_relation`
                        let rel_idx =
                            usize::from(cur.next_u8().unwrap_or(0)) % registered_relations.len();
                        let (rel_h, _) = registered_relations[rel_idx];
                        let src =
                            ffi_pool[usize::from(cur.next_u8().unwrap_or(0)) % ffi_pool.len()];
                        let dst =
                            ffi_pool[usize::from(cur.next_u8().unwrap_or(0)) % ffi_pool.len()];
                        // SAFETY: Active `cmds_ptr`.
                        let _ = black_box(unsafe {
                            host_cmd_remove_relation(cmds_ptr, src, rel_h, dst)
                        });
                    }
                    4 => {
                        // `cmd_spawn_empty` (bounded pool growth)
                        if tracked_entities.len() + newly_spawned.len() < 28 {
                            let mut out = FfiEntity::NULL;
                            // SAFETY: Active `cmds_ptr` and writable `out`.
                            if unsafe { host_cmd_spawn_empty(cmds_ptr, &raw mut out) }
                                == FfiResult::OK
                                && let Some(e) = Entity::try_from_bits(out.to_bits())
                            {
                                newly_spawned.push(e);
                                ffi_pool.push(out);
                            }
                        }
                    }
                    5 => {
                        // `cmd_despawn` or schedule synchronous `world.despawn`
                        let pick = usize::from(cur.next_u8().unwrap_or(0));
                        if pick & 1 == 0 {
                            let ent = ffi_pool[pick % ffi_pool.len()];
                            // SAFETY: Active `cmds_ptr`.
                            let _ = black_box(unsafe { host_cmd_despawn(cmds_ptr, ent) });
                        } else if !tracked_entities.is_empty() {
                            sync_despawns.push(tracked_entities[pick % tracked_entities.len()]);
                        }
                    }
                    6 => {
                        // `cmd_checkpoint`
                        let mut cp = 0usize;
                        // SAFETY: Active `cmds_ptr` and writable `cp`.
                        if unsafe { host_cmd_checkpoint(cmds_ptr, &raw mut cp) } == FfiResult::OK {
                            checkpoints.push(cp);
                        }
                    }
                    _ => {
                        // `cmd_rollback` (valid recorded checkpoint or out-of-range token)
                        let sel = cur.next_u8().unwrap_or(0);
                        let target_cp = if sel & 3 == 0 || checkpoints.is_empty() {
                            usize::from(sel).saturating_add(100)
                        } else {
                            let idx = usize::from(sel) % checkpoints.len();
                            let cp = checkpoints[idx];
                            checkpoints.truncate(idx + 1);
                            cp
                        };
                        // SAFETY: Active `cmds_ptr`.
                        let _ = black_box(unsafe { host_cmd_rollback(cmds_ptr, target_cp) });
                    }
                }
            }
        }

        let commit_flag = cur.next_u8().unwrap_or(0);
        if !commit_flag.is_multiple_of(5) {
            storage.apply_to_world(&mut world, &snapshot);
            world.flush();
            tracked_entities.extend(newly_spawned);
        } else {
            storage.clear();
        }

        for e in sync_despawns {
            if world.entities().contains(e) {
                let _ = world.despawn(e);
                world.flush();
            }
        }

        verify_graph_invariants(&world, &rel_handles, &tracked_entities);
    }

    // Final despawn sweep verifying all `CASCADE_DELETE` / `CLEANUP_REMOVE` hooks clear cleanly.
    for &e in &tracked_entities {
        if world.entities().contains(e) {
            let _ = world.despawn(e);
            world.flush();
        }
    }
    verify_graph_invariants(&world, &rel_handles, &tracked_entities);
    if let Some(graph) = world.get_resource::<ModRelationGraph>() {
        for &rel in &rel_handles {
            assert_eq!(
                graph.edge_count(rel),
                0,
                "relation {:?} retained edges after despawning all tracked entities",
                rel
            );
        }
    }
});
