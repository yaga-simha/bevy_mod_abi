#![forbid(unsafe_code)]

//! Safe Rust (`#![forbid(unsafe_code)]`, zero `bevy*` deps) guest DSO implementing the complete
//! `bevy_bql` ABI `0.3.0` differential wire parity contract (`v1` default, `v2` via `--features v2`).

use core::ffi::CStr;
use core::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use bevy_abi_mod_guest::prelude::*;

const NUM_ROOTS: usize = 4;
const BRANCHES_PER_ROOT: usize = 4;
const NUM_BRANCHES: usize = NUM_ROOTS * BRANCHES_PER_ROOT;
const LEAVES_PER_BRANCH: usize = 36;
const NUM_LEAVES: usize = NUM_BRANCHES * LEAVES_PER_BRANCH;
const STATE_MAGIC: u64 = 0x4251_4C50_5354_4154;
const STATE_BLOB_LEN: usize = 9 * 8;

// ============================================================================
// Host-Registered Singleton Resources (`#[repr(C)]`, zero-padding `ModComponent`)
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::SimClock")]
struct SimClock {
    frame: u32,
    schema_gen: u32,
    checksum_seed: u64,
}

impl GuestComponent for SimClock {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::FrameAccumulator")]
struct FrameAccumulator {
    readonly_sum: u64,
    readonly_count: u64,
    committed_tx_count: u32,
    rolled_back_tx_count: u32,
    interned_tag_alpha: u32,
    interned_tag_beta: u32,
    order_trace: u64,
}

impl GuestComponent for FrameAccumulator {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

// ============================================================================
// Guest Dynamic `AbiPod` Components with `SCHEMA_FINGERPRINT` & `BAM_FIELDS`
// ============================================================================

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::NodeState")]
struct NodeStateV1 {
    node_id: u32,
    local_val: i32,
    tag_sym: u32,
}

impl GuestComponent for NodeStateV1 {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[cfg(feature = "v2")]
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::NodeState")]
struct NodeStateV2 {
    node_id: u32,
    local_val: i32,
    tag_sym: u32,
    boost: i32,
}

#[cfg(feature = "v2")]
impl GuestComponent for NodeStateV2 {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[cfg(not(feature = "v2"))]
type ActiveNodeState = NodeStateV1;
#[cfg(feature = "v2")]
type ActiveNodeState = NodeStateV2;

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::RootAura")]
struct RootAura {
    effective_val: i32,
    aura_tag: u32,
}

impl GuestComponent for RootAura {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::WorldMetric")]
struct WorldMetric {
    world_val: i32,
    cascade_depth: i32,
    parent_tag: u32,
    energy: f32,
}

impl GuestComponent for WorldMetric {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, ModComponent)]
#[mod_component(name = "bql_parity::TxToken")]
struct TxToken {
    frame_created: u32,
    kind_sym: u32,
}

impl GuestComponent for TxToken {
    const NAME: &'static CStr = <Self as ModComponent>::COMPONENT_CSTR;
    const UUID: ComponentUuid = <Self as ModComponent>::UUID;
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, AbiPod)]
struct EdgeWeight {
    frame: u32,
    weight: i32,
}

// ============================================================================
// Per-Instance Mod State (Zero Process Globals, Reload-Safe Serialization)
// ============================================================================

#[derive(Default, Clone, Copy)]
struct LifecycleState {
    bootstrapped: bool,
    roots: [FfiEntity; NUM_ROOTS],
    perm_sat: FfiEntity,
    active_anchor: FfiEntity,
    active_sat: FfiEntity,
}

impl LifecycleState {
    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(STATE_BLOB_LEN);
        out.extend_from_slice(&STATE_MAGIC.to_le_bytes());
        out.extend_from_slice(&(u64::from(self.bootstrapped)).to_le_bytes());
        for root in self.roots {
            out.extend_from_slice(&root.to_bits().to_le_bytes());
        }
        out.extend_from_slice(&self.perm_sat.to_bits().to_le_bytes());
        out.extend_from_slice(&self.active_anchor.to_bits().to_le_bytes());
        out.extend_from_slice(&self.active_sat.to_bits().to_le_bytes());
        out
    }

    fn decode(bytes: &[u8]) -> Result<Self, FfiResult> {
        if bytes.len() != STATE_BLOB_LEN {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let read_u64 = |idx: usize| -> u64 {
            let start = idx * 8;
            let mut arr = [0u8; 8];
            arr.copy_from_slice(&bytes[start..start + 8]);
            u64::from_le_bytes(arr)
        };
        if read_u64(0) != STATE_MAGIC {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        Ok(Self {
            bootstrapped: read_u64(1) != 0,
            roots: [
                FfiEntity::from_bits(read_u64(2)),
                FfiEntity::from_bits(read_u64(3)),
                FfiEntity::from_bits(read_u64(4)),
                FfiEntity::from_bits(read_u64(5)),
            ],
            perm_sat: FfiEntity::from_bits(read_u64(6)),
            active_anchor: FfiEntity::from_bits(read_u64(7)),
            active_sat: FfiEntity::from_bits(read_u64(8)),
        })
    }
}

#[derive(Clone, Copy)]
struct ParityHandles {
    child_of: BamComponent,
    node_state: BamComponent,
    root_aura: BamComponent,
    world_metric: BamComponent,
    tx_token: BamComponent,
    rel_owned_by: BamComponent,
    sym_root: u32,
    sym_branch: u32,
    sym_leaf: u32,
    sym_alpha: u32,
    sym_beta: u32,
    sym_tx_ok: u32,
    #[cfg(feature = "v2")]
    sym_v2: u32,
}

#[cfg(feature = "v2")]
#[inline]
const fn compute_migrated_boost(node_id: u32, local_val: i32) -> i32 {
    (local_val / 4) + ((node_id % 64) as i32) * 3 + 7
}

#[inline]
const fn make_initial_node(node_id: u32, local_val: i32, tag_sym: u32) -> ActiveNodeState {
    #[cfg(not(feature = "v2"))]
    {
        NodeStateV1 {
            node_id,
            local_val,
            tag_sym,
        }
    }
    #[cfg(feature = "v2")]
    {
        NodeStateV2 {
            node_id,
            local_val,
            tag_sym,
            boost: compute_migrated_boost(node_id, local_val),
        }
    }
}

#[derive(Default)]
struct BqlParityMod {
    lifecycle: Arc<Mutex<LifecycleState>>,
}

impl GuestPlugin for BqlParityMod {
    const NAME: &'static CStr = c"bql_wire_parity";
    #[cfg(not(feature = "v2"))]
    const VERSION: &'static CStr = c"1.0.0";
    #[cfg(feature = "v2")]
    const VERSION: &'static CStr = c"2.0.0";

    fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
        // 1. Register or migrate dynamic `AbiPod` components with `SCHEMA_FINGERPRINT` & `BAM_FIELDS`.
        let node_state = match register_or_migrate_node_state(app) {
            Ok(h) => h,
            Err(err) => return err,
        };
        let root_aura = match app.register_dynamic_component::<RootAura>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let world_metric = match app.register_dynamic_component::<WorldMetric>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let tx_token = match app.register_dynamic_component::<TxToken>() {
            Ok(h) => h,
            Err(err) => return err,
        };

        // Verify layout metadata via `resolve_component_by_uuid` + `verify_component_layout`.
        let resolved_node =
            match app.resolve_component_by_uuid(<ActiveNodeState as ModComponent>::UUID) {
                Ok(h) => h,
                Err(err) => return err,
            };
        if resolved_node != node_state {
            return FfiResult::TYPE_MISMATCH;
        }
        if let Err(err) = app.verify_component_layout::<ActiveNodeState>() {
            return err;
        }
        if let Err(err) = app.verify_component_layout::<WorldMetric>() {
            return err;
        }

        // 2. Register `ChildOf` handle and typed `RELATION` (`EXCLUSIVE | CASCADE_DELETE`).
        let child_of = match app.resolve_component::<FfiChildOf>() {
            Ok(h) => h,
            Err(err) => return err,
        };
        let rel_owned_by = match app.register_relation(
            RelationDescBuilder::new("bql_parity::OwnedBy")
                .exclusive()
                .cascade_delete()
                .with_payload::<EdgeWeight>()
                .max_degree(1),
        ) {
            Ok(h) => h,
            Err(err) => return err,
        };

        // 3. Intern canonical `InternedStr` symbols monotonically.
        let sym_root = match app.intern_symbol("bql::tag::root") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        let sym_branch = match app.intern_symbol("bql::tag::branch") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        let sym_leaf = match app.intern_symbol("bql::tag::leaf") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        let sym_alpha = match app.intern_symbol("bql::tag::alpha") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        let sym_beta = match app.intern_symbol("bql::tag::beta") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        let sym_tx_ok = match app.intern_symbol("bql::tx::committed") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };
        #[cfg(feature = "v2")]
        let sym_v2 = match app.intern_symbol("bql::tag::v2_boosted") {
            Ok(s) => s.to_raw(),
            Err(err) => return err,
        };

        let handles = ParityHandles {
            child_of,
            node_state,
            root_aura,
            world_metric,
            tx_token,
            rel_owned_by,
            sym_root,
            sym_branch,
            sym_leaf,
            sym_alpha,
            sym_beta,
            sym_tx_ok,
            #[cfg(feature = "v2")]
            sym_v2,
        };

        // 4. Configure decoupled `SCHEDULE` sets in `FixedUpdate`:
        //    `bql_pipeline` (`SEQUENCE_WEAK` / `.chain_weak()`) -> `bql_finalize` (`SEQUENCE`).
        let set_pipeline = match app.configure_schedule_set(
            ScheduleSetBuilder::new("bql_pipeline")
                .schedule(ScheduleId::FIXED_UPDATE)
                .chain_weak(),
        ) {
            Ok(s) => s,
            Err(err) => return err,
        };
        let set_finalize = match app.configure_schedule_set(
            ScheduleSetBuilder::new("bql_finalize")
                .schedule(ScheduleId::FIXED_UPDATE)
                .sequence()
                .after(set_pipeline),
        ) {
            Ok(s) => s,
            Err(err) => return err,
        };

        // 5. Register all 5 system handles via `register_system_handle` and mount into sets.
        let lifecycle_s1 = Arc::clone(&self.lifecycle);
        let sys_01 = match app
            .system(c"sys_01_lifecycle_tx_rel")
            .resource_read::<SimClock>()
            .resource_write::<FrameAccumulator>()
            .register_handle(move |ctx| run_sys_01_lifecycle(ctx, handles, &lifecycle_s1))
        {
            Ok(h) => h,
            Err(err) => return err,
        };

        let sys_02 = match app
            .system(c"sys_02_par_mutate_nodes")
            .query(|q| q.write::<ActiveNodeState>())
            .resource_read::<SimClock>()
            .resource_write::<FrameAccumulator>()
            .register_handle(move |ctx| run_sys_02_par_mutate(ctx, handles))
        {
            Ok(h) => h,
            Err(err) => return err,
        };

        let sys_03 = match app
            .system(c"sys_03_update_roots")
            .query(|q| q.write::<RootAura>().read::<ActiveNodeState>())
            .resource_read::<SimClock>()
            .resource_write::<FrameAccumulator>()
            .register_handle(run_sys_03_update_roots)
        {
            Ok(h) => h,
            Err(err) => return err,
        };

        let sys_04 = match app
            .system(c"sys_04_cascade_hierarchy")
            .query(|q| {
                q.write::<WorldMetric>()
                    .read::<ActiveNodeState>()
                    .optional_read::<RootAura>()
            })
            .resource_read::<SimClock>()
            .resource_write::<FrameAccumulator>()
            .register_handle(run_sys_04_cascade)
        {
            Ok(h) => h,
            Err(err) => return err,
        };

        let sys_05 = match app
            .system(c"sys_05_par_readonly_reduce")
            .query(|q| q.read::<ActiveNodeState>().read::<WorldMetric>())
            .resource_write::<FrameAccumulator>()
            .register_handle(run_sys_05_par_readonly_reduce)
        {
            Ok(h) => h,
            Err(err) => return err,
        };

        for sys_h in [sys_01, sys_02, sys_03, sys_04] {
            let status = app.mount_system(sys_h, ScheduleId::FIXED_UPDATE, set_pipeline);
            if !status.is_ok() {
                return status;
            }
        }
        let status = app.mount_system(sys_05, ScheduleId::FIXED_UPDATE, set_finalize);
        if !status.is_ok() {
            return status;
        }

        FfiResult::OK
    }

    fn save_state(&self) -> Result<Vec<u8>, FfiResult> {
        let guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
        Ok(guard.encode())
    }

    fn restore_state(&mut self, bytes: &[u8]) -> FfiResult {
        if bytes.is_empty() {
            return FfiResult::OK;
        }
        match LifecycleState::decode(bytes) {
            Ok(decoded) => {
                let mut guard = self.lifecycle.lock().unwrap_or_else(|e| e.into_inner());
                *guard = decoded;
                FfiResult::OK
            }
            Err(err) => err,
        }
    }
}

#[cfg(not(feature = "v2"))]
fn register_or_migrate_node_state(app: &mut GuestApp<'_>) -> Result<BamComponent, FfiResult> {
    app.register_dynamic_component::<NodeStateV1>()
}

#[cfg(feature = "v2")]
fn register_or_migrate_node_state(app: &mut GuestApp<'_>) -> Result<BamComponent, FfiResult> {
    match app.resolve_component_named(Some(c"bql_parity::NodeState"), None) {
        Ok(existing) => {
            let layout = app.component_layout_info(existing)?;
            if layout.schema_fingerprint == NodeStateV1::SCHEMA_FINGERPRINT {
                let rc =
                    app.migrate_component_column::<NodeStateV1, NodeStateV2, _>(existing, |old| {
                        Ok(NodeStateV2 {
                            node_id: old.node_id,
                            local_val: old.local_val,
                            tag_sym: old.tag_sym,
                            boost: compute_migrated_boost(old.node_id, old.local_val),
                        })
                    });
                if !rc.is_ok() {
                    return Err(rc);
                }
                Ok(existing)
            } else if layout.schema_fingerprint == NodeStateV2::SCHEMA_FINGERPRINT {
                app.register_dynamic_component::<NodeStateV2>()
            } else {
                Err(FfiResult::TYPE_MISMATCH)
            }
        }
        Err(FfiResult::NOT_FOUND) => app.register_dynamic_component::<NodeStateV2>(),
        Err(err) => Err(err),
    }
}

fn run_sys_01_lifecycle(
    ctx: &mut SystemContext<'_>,
    h: ParityHandles,
    lifecycle_arc: &Arc<Mutex<LifecycleState>>,
) -> FfiResult {
    let frame = match ctx.component_resource::<SimClock>(0) {
        Some(c) => c.frame,
        None => return FfiResult::NOT_FOUND,
    };

    let mut state = *lifecycle_arc.lock().unwrap_or_else(|e| e.into_inner());
    let mut rolled_back_inc: u32 = 0;
    let mut committed_inc: u32 = 0;

    if !state.bootstrapped {
        let mut branches = [FfiEntity::NULL; NUM_BRANCHES];
        let mut root0 = FfiEntity::NULL;

        {
            let cmds = ctx.commands();
            for (r, root_slot) in state.roots.iter_mut().enumerate() {
                let node = make_initial_node(r as u32, ((r as i32) + 1) * 10, h.sym_root);
                let aura = RootAura {
                    effective_val: 0,
                    aura_tag: h.sym_root,
                };
                let metric = WorldMetric {
                    world_val: 0,
                    cascade_depth: 0,
                    parent_tag: h.sym_root,
                    energy: 0.0,
                };
                let ent = match cmds.spawn(&[
                    SpawnEntry::new(h.node_state, &node),
                    SpawnEntry::new(h.root_aura, &aura),
                    SpawnEntry::new(h.world_metric, &metric),
                ]) {
                    Ok(e) => e,
                    Err(err) => return err,
                };
                *root_slot = ent;
                if r == 0 {
                    root0 = ent;
                }
            }

            for (b, branch_slot) in branches.iter_mut().enumerate() {
                let parent = state.roots[b / BRANCHES_PER_ROOT];
                let child_of = FfiChildOf::new(parent);
                let node =
                    make_initial_node((NUM_ROOTS + b) as u32, ((b as i32) + 1) * 3, h.sym_branch);
                let metric = WorldMetric {
                    world_val: 0,
                    cascade_depth: 1,
                    parent_tag: h.sym_root,
                    energy: 0.0,
                };
                let ent = match cmds.spawn(&[
                    SpawnEntry::new(h.child_of, &child_of),
                    SpawnEntry::new(h.node_state, &node),
                    SpawnEntry::new(h.world_metric, &metric),
                ]) {
                    Ok(e) => e,
                    Err(err) => return err,
                };
                *branch_slot = ent;
            }

            for l in 0..NUM_LEAVES {
                let parent = branches[l / LEAVES_PER_BRANCH];
                let child_of = FfiChildOf::new(parent);
                let tag_sym = if l % 2 == 0 { h.sym_alpha } else { h.sym_beta };
                let node = make_initial_node(
                    (NUM_ROOTS + NUM_BRANCHES + l) as u32,
                    ((l % 17) as i32) - 8,
                    tag_sym,
                );
                let metric = WorldMetric {
                    world_val: 0,
                    cascade_depth: 2,
                    parent_tag: h.sym_branch,
                    energy: 0.0,
                };
                let _leaf_ent = match cmds.spawn(&[
                    SpawnEntry::new(h.child_of, &child_of),
                    SpawnEntry::new(h.node_state, &node),
                    SpawnEntry::new(h.world_metric, &metric),
                ]) {
                    Ok(e) => e,
                    Err(err) => return err,
                };
            }

            let perm_tok = TxToken {
                frame_created: frame,
                kind_sym: h.sym_tx_ok,
            };
            let perm_sat = match cmds.spawn(&[SpawnEntry::new(h.tx_token, &perm_tok)]) {
                Ok(e) => e,
                Err(err) => return err,
            };
            let init_edge = EdgeWeight { frame, weight: 10 };
            let rc = cmds.insert_relation_with_payload(perm_sat, h.rel_owned_by, root0, &init_edge);
            if !rc.is_ok() {
                return rc;
            }
            state.perm_sat = perm_sat;
            state.bootstrapped = true;
        }

        // Bind named persistent `OBJECT` `"bql_parity::root_config"` to Root #0.
        let bind_rc = ctx.bind_named_object("bql_parity::root_config", root0);
        if !bind_rc.is_ok() {
            return bind_rc;
        }
    } else {
        let cmds = ctx.commands();

        // 1. Failing `ATOMIC` transaction -> `cmd_checkpoint` + `cmd_rollback` every frame.
        let rollback_res: Result<(), FfiResult> = cmds.transaction(|tx| {
            let bogus_tok = TxToken {
                frame_created: frame,
                kind_sym: h.sym_beta,
            };
            let bogus = tx.spawn(&[SpawnEntry::new(h.tx_token, &bogus_tok)])?;
            let bogus_edge = EdgeWeight {
                frame,
                weight: -999,
            };
            let rc =
                tx.insert_relation_with_payload(bogus, h.rel_owned_by, state.roots[0], &bogus_edge);
            if !rc.is_ok() {
                return Err(rc);
            }
            // Deliberately fail condition so `GuestTransaction` rolls back cleanly!
            Err(FfiResult::INVALID_STATE)
        });
        if rollback_res != Err(FfiResult::INVALID_STATE) {
            return FfiResult::INTERNAL_ERROR;
        }
        rolled_back_inc = 1;

        // 2. Refresh persistent `EXCLUSIVE` relation target every 10 frames (rotates across roots).
        if frame % 10 == 3 && !state.perm_sat.is_null() {
            let target = state.roots[((frame / 10) as usize) % NUM_ROOTS];
            let edge = EdgeWeight {
                frame,
                weight: (frame as i32) + 100,
            };
            let rc =
                cmds.insert_relation_with_payload(state.perm_sat, h.rel_owned_by, target, &edge);
            if !rc.is_ok() {
                return rc;
            }
        }

        // 3. Five-frame transactional relation & cascade-delete lifecycle.
        if frame % 5 == 0 {
            let committed = cmds.transaction(|tx| {
                let anchor_tok = TxToken {
                    frame_created: frame,
                    kind_sym: h.sym_alpha,
                };
                let anchor = tx.spawn(&[SpawnEntry::new(h.tx_token, &anchor_tok)])?;

                let sat_tok = TxToken {
                    frame_created: frame,
                    kind_sym: h.sym_tx_ok,
                };
                let sat = tx.spawn(&[SpawnEntry::new(h.tx_token, &sat_tok)])?;

                // Attach initially to `roots[1]`, then replace via `EXCLUSIVE` to `anchor`.
                let first_edge = EdgeWeight {
                    frame,
                    weight: (frame as i32) * 2,
                };
                let rc1 = tx.insert_relation_with_payload(
                    sat,
                    h.rel_owned_by,
                    state.roots[1],
                    &first_edge,
                );
                if !rc1.is_ok() {
                    return Err(rc1);
                }
                let final_edge = EdgeWeight {
                    frame,
                    weight: (frame as i32) * 3 + 1,
                };
                let rc2 = tx.insert_relation_with_payload(sat, h.rel_owned_by, anchor, &final_edge);
                if !rc2.is_ok() {
                    return Err(rc2);
                }
                Ok((anchor, sat))
            });
            match committed {
                Ok((anchor, sat)) => {
                    state.active_anchor = anchor;
                    state.active_sat = sat;
                    committed_inc = 1;
                }
                Err(err) => return err,
            }
        } else if frame % 5 == 2 && !state.active_anchor.is_null() && !state.active_sat.is_null() {
            // Explicitly remove relation edge, spawn a second satellite, then re-attach both.
            let rm_rc = cmds.remove_relation(state.active_sat, h.rel_owned_by, state.active_anchor);
            if !rm_rc.is_ok() {
                return rm_rc;
            }
            let extra_tok = TxToken {
                frame_created: frame,
                kind_sym: h.sym_leaf,
            };
            let extra_sat = match cmds.spawn(&[SpawnEntry::new(h.tx_token, &extra_tok)]) {
                Ok(e) => e,
                Err(err) => return err,
            };
            let extra_edge = EdgeWeight { frame, weight: 42 };
            let rc1 = cmds.insert_relation_with_payload(
                extra_sat,
                h.rel_owned_by,
                state.active_anchor,
                &extra_edge,
            );
            if !rc1.is_ok() {
                return rc1;
            }
            let reattach_edge = EdgeWeight {
                frame,
                weight: (frame as i32) + 7,
            };
            let rc2 = cmds.insert_relation_with_payload(
                state.active_sat,
                h.rel_owned_by,
                state.active_anchor,
                &reattach_edge,
            );
            if !rc2.is_ok() {
                return rc2;
            }
        } else if frame % 5 == 4 && !state.active_anchor.is_null() {
            // Despawn anchor -> `BAM_RELATION_FLAG_CASCADE_DELETE` cascades to all attached satellites!
            let despawn_rc = cmds.despawn(state.active_anchor);
            if !despawn_rc.is_ok() {
                return despawn_rc;
            }
            state.active_anchor = FfiEntity::NULL;
            state.active_sat = FfiEntity::NULL;
        }
    }

    *lifecycle_arc.lock().unwrap_or_else(|e| e.into_inner()) = state;

    let Some(acc) = ctx.component_resource_mut::<FrameAccumulator>(1) else {
        return FfiResult::NOT_FOUND;
    };
    acc.committed_tx_count = acc.committed_tx_count.wrapping_add(committed_inc);
    acc.rolled_back_tx_count = acc.rolled_back_tx_count.wrapping_add(rolled_back_inc);
    acc.interned_tag_alpha = h.sym_alpha;
    acc.interned_tag_beta = h.sym_beta;
    acc.order_trace = 1;
    FfiResult::OK
}

fn run_sys_02_par_mutate(ctx: &mut SystemContext<'_>, handles: ParityHandles) -> FfiResult {
    let frame = match ctx.component_resource::<SimClock>(0) {
        Some(c) => c.frame,
        None => return FfiResult::NOT_FOUND,
    };
    #[cfg(not(feature = "v2"))]
    let _ = handles;

    let par_rc = ctx.iter().par_try_for_each(|mut chunk| {
        let Some(nodes) = chunk.component_mut::<ActiveNodeState>(0) else {
            return FfiResult::TYPE_MISMATCH;
        };
        for node in nodes {
            let delta = ((frame.wrapping_add(node.node_id) % 11) as i32) - 5;
            #[cfg(not(feature = "v2"))]
            {
                let shifted = node.local_val + delta + 2000;
                node.local_val = (shifted % 2000) - 1000;
            }
            #[cfg(feature = "v2")]
            {
                let b_delta = ((frame % 7) as i32) - 3;
                let b_shifted = node.boost + b_delta + 1000;
                node.boost = (b_shifted % 1000) - 500;
                let shifted = node.local_val + delta + (node.boost / 8) + 2000;
                node.local_val = (shifted % 2000) - 1000;
                if frame == 250 && node.node_id % 13 == 0 {
                    node.tag_sym = handles.sym_v2;
                }
            }
        }
        FfiResult::OK
    });
    if !par_rc.is_ok() {
        return par_rc;
    }

    let Some(acc) = ctx.component_resource_mut::<FrameAccumulator>(1) else {
        return FfiResult::NOT_FOUND;
    };
    acc.order_trace = (acc.order_trace << 4) | 2;
    FfiResult::OK
}

fn run_sys_03_update_roots(ctx: &mut SystemContext<'_>) -> FfiResult {
    let frame = match ctx.component_resource::<SimClock>(0) {
        Some(c) => c.frame,
        None => return FfiResult::NOT_FOUND,
    };

    while let Some(mut chunk) = ctx.iter().next_chunk_view() {
        let Some((nodes, auras)) = chunk.component_read_write::<ActiveNodeState, RootAura>(1, 0)
        else {
            return FfiResult::TYPE_MISMATCH;
        };
        for (aura, node) in auras.iter_mut().zip(nodes.iter()) {
            #[cfg(not(feature = "v2"))]
            {
                aura.effective_val = node.local_val * 2 + (frame % 13) as i32;
            }
            #[cfg(feature = "v2")]
            {
                aura.effective_val = node.local_val * 2 + node.boost + (frame % 13) as i32;
            }
            aura.aura_tag = node.tag_sym;
        }
    }

    let Some(acc) = ctx.component_resource_mut::<FrameAccumulator>(1) else {
        return FfiResult::NOT_FOUND;
    };
    acc.order_trace = (acc.order_trace << 4) | 3;
    FfiResult::OK
}

fn run_sys_04_cascade(ctx: &mut SystemContext<'_>) -> FfiResult {
    let _frame = match ctx.component_resource::<SimClock>(0) {
        Some(c) => c.frame,
        None => return FfiResult::NOT_FOUND,
    };

    let mut ents_buf: Vec<FfiEntity> = Vec::new();
    let mut nodes_buf: Vec<ActiveNodeState> = Vec::new();
    let mut own_auras_buf: Vec<RootAura> = Vec::new();
    let mut out_metrics: Vec<WorldMetric> = Vec::new();

    {
        let iter = ctx.iter();
        while iter.next_chunk().is_some() {
            ents_buf.clear();
            ents_buf.extend_from_slice(iter.entities());

            nodes_buf.clear();
            {
                let node_slice = iter.readonly_slice(1);
                let Some(typed_nodes) = node_slice.as_typed_slice::<ActiveNodeState>() else {
                    return FfiResult::TYPE_MISMATCH;
                };
                nodes_buf.extend_from_slice(typed_nodes);
            }

            own_auras_buf.clear();
            {
                let aura_slice = iter.readonly_slice(2);
                if let Some(typed_auras) = aura_slice.as_typed_slice::<RootAura>() {
                    own_auras_buf.extend_from_slice(typed_auras);
                }
            }

            out_metrics.clear();
            out_metrics.reserve(ents_buf.len());

            for (idx, &entity) in ents_buf.iter().enumerate() {
                let node = nodes_buf[idx];

                // Multi-hop `UP ChildOf CASCADE` lookup for ancestor `RootAura` (term 2).
                let root_bonus = match iter.up_cascade::<RootAura>(BamComponent::INVALID, entity, 2)
                {
                    Ok(Some((_owner, aura))) => aura.effective_val,
                    Ok(None) => {
                        if idx < own_auras_buf.len() {
                            own_auras_buf[idx].effective_val
                        } else {
                            0
                        }
                    }
                    Err(err) => return err,
                };

                // Walk upward along `ChildOf` via `iter_up_cascade` over `NodeState` (term 1).
                let mut depth: i32 = 0;
                let mut ancestor_sum: i32 = 0;
                let mut parent_tag: u32 = node.tag_sym;
                let mut cur = entity;
                while depth < 16 {
                    match iter.up_cascade::<ActiveNodeState>(BamComponent::INVALID, cur, 1) {
                        Ok(Some((owner, ancestor_node))) => {
                            if depth == 0 {
                                parent_tag = ancestor_node.tag_sym;
                            }
                            ancestor_sum += ancestor_node.local_val;
                            #[cfg(feature = "v2")]
                            {
                                ancestor_sum += ancestor_node.boost;
                            }
                            depth += 1;
                            cur = owner;
                        }
                        Ok(None) => break,
                        Err(err) => return err,
                    }
                }

                #[cfg(not(feature = "v2"))]
                let world_val = root_bonus + ancestor_sum + node.local_val;
                #[cfg(feature = "v2")]
                let world_val = root_bonus + ancestor_sum + node.local_val + node.boost;

                let energy_steps = ((world_val as u32) & 255) as i32 + depth * 4;
                let energy = (energy_steps as f32) * 0.25_f32;

                out_metrics.push(WorldMetric {
                    world_val,
                    cascade_depth: depth,
                    parent_tag,
                    energy,
                });
            }

            let mut wm_slice = iter.mutable_slice(0);
            let Some(wm_typed) = wm_slice.as_typed_slice_mut::<WorldMetric>() else {
                return FfiResult::TYPE_MISMATCH;
            };
            wm_typed.copy_from_slice(&out_metrics);
        }
    }

    let Some(acc) = ctx.component_resource_mut::<FrameAccumulator>(1) else {
        return FfiResult::NOT_FOUND;
    };
    acc.order_trace = (acc.order_trace << 4) | 4;
    FfiResult::OK
}

fn run_sys_05_par_readonly_reduce(ctx: &mut SystemContext<'_>) -> FfiResult {
    let total_count = match ctx.iter().count() {
        Ok(c) => c,
        Err(err) => return err,
    };

    let shared_sum = AtomicU64::new(0);
    let par_rc = ctx.par_try_chunks_readonly(|chunk| {
        let Some(nodes) = chunk.component::<ActiveNodeState>(0) else {
            return FfiResult::TYPE_MISMATCH;
        };
        let Some(metrics) = chunk.component::<WorldMetric>(1) else {
            return FfiResult::TYPE_MISMATCH;
        };
        let mut local_sum: u64 = 0;
        for (node, metric) in nodes.iter().zip(metrics.iter()) {
            let energy_bits = metric.energy.to_bits();
            #[allow(unused_mut)]
            let mut row_val: u64 = (u64::from(node.node_id) << 32)
                ^ u64::from(node.local_val as u32)
                ^ (u64::from(metric.world_val as u32) << 16)
                ^ u64::from(metric.cascade_depth as u32)
                ^ (u64::from(node.tag_sym) << 8)
                ^ (u64::from(metric.parent_tag) << 24)
                ^ u64::from(energy_bits);
            #[cfg(feature = "v2")]
            {
                row_val ^= u64::from(node.boost as u32) << 40;
            }
            local_sum = local_sum.wrapping_add(row_val);
        }
        shared_sum.fetch_add(local_sum, Ordering::Relaxed);
        FfiResult::OK
    });
    if !par_rc.is_ok() {
        return par_rc;
    }

    let Some(acc) = ctx.component_resource_mut::<FrameAccumulator>(0) else {
        return FfiResult::NOT_FOUND;
    };
    acc.readonly_sum = shared_sum.load(Ordering::Relaxed);
    acc.readonly_count = total_count as u64;
    acc.order_trace = (acc.order_trace << 4) | 5;
    FfiResult::OK
}

export_mod!(plugin: BqlParityMod);
