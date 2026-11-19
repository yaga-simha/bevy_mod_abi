//! ABI `0.3.0` generic entity relationship storage, validation, cycle detection, and Bevy `0.20`
//! lifecycle hooks (`register_relation`, `cmd_insert_relation`, `cmd_remove_relation`).
//!
//! # Architecture
//! - Relationship descriptors (`BamRelationDesc`) are stored in [`ModRegistry`] alongside a
//!   zero-sized `StorageType::SparseSet` immutable Bevy marker component registered with
//!   `on_despawn` and `on_remove` hooks.
//! - Live relationship edges and optional POD edge payloads live in the per-`World`
//!   [`ModRelationGraph`] resource, indexed symmetrically by outgoing `(source -> [(target, data)])`
//!   and incoming `(target -> [source])` adjacency maps for `O(deg)` enforcement and cleanup.
//! - Participating entities carry the relation's Bevy marker component iff they have at least one
//!   incident (incoming or outgoing) edge, so direct `world.despawn(e)`, `cmd_despawn`,
//!   `cmd_despawn_all`, or `remove_by_id` automatically trigger `CASCADE_DESPAWN`/`CASCADE_DELETE`
//!   and `CLEANUP_REMOVE` without scanning unrelated entities.

use crate::ffi::commands::HostCommands;
use crate::ffi::register::validate_active_load_ctx;
use crate::handle_header::{COMMANDS_MAGIC, validate_handle_mut};
use crate::registry::catalog::{ModRegistry, RegisteredRelation, RegistrySnapshot};
use bevy_abi_mod_sys::{
    BAM_RELATION_FLAG_EXCLUSIVE, BamCommands, BamComponent, BamLoadCtx, BamRelationDesc, FfiEntity,
    FfiResult, read_descriptor,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::Entity;
use bevy_ecs::lifecycle::HookContext;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::{DeferredWorld, World};
use bevy_platform::collections::{HashMap, HashSet};
use core::alloc::Layout;
use core::ffi::{CStr, c_char, c_void};
use std::collections::VecDeque;

/// Policy controlling how [`ModRelationGraph`] handles deferred relation command failures
/// (such as [`FfiResult::CYCLE_DETECTED`] or [`FfiResult::CAPACITY_EXCEEDED`]) during
/// [`crate::HostCommandsStorage::apply_to_world`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RelationFlushPolicy {
    /// Emit a `tracing::warn!` diagnostic, record a [`RelationFlushError`], leave graph invariants
    /// intact, and continue flushing subsequent commands.
    #[default]
    LogAndSkip,
    /// Silently record a [`RelationFlushError`] without logging, leave graph invariants intact,
    /// and continue flushing subsequent commands.
    RecordOnly,
    /// Emit a `tracing::warn!` diagnostic, record a [`RelationFlushError`], leave graph invariants
    /// intact, and skip all remaining deferred relation commands in the current flush batch.
    AbortRemainingRelations,
}

/// Diagnostic record captured when a deferred relationship command is rejected during flush.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationFlushError {
    /// Registered 1-based [`BamComponent`] handle of the target relationship.
    pub relation: BamComponent,
    /// Source entity of the rejected edge operation.
    pub entity: Entity,
    /// Target entity of the rejected edge operation.
    pub target: Entity,
    /// Exact [`FfiResult`] error code explaining why the operation was rejected.
    pub status: FfiResult,
}

/// Read-only view of an outgoing relationship edge and its raw POD payload bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RelationEdgeView<'a> {
    /// Target entity of the directed edge `(source -> target)`.
    pub target: Entity,
    /// Raw edge payload bytes (empty slice when `data_size == 0`).
    pub data: &'a [u8],
}

#[derive(Clone, Debug)]
struct RelationEdgeTable {
    meta: RegisteredRelation,
    outgoing: HashMap<Entity, Vec<(Entity, Box<[u8]>)>>,
    incoming: HashMap<Entity, Vec<Entity>>,
}

impl RelationEdgeTable {
    fn new(meta: RegisteredRelation) -> Self {
        Self {
            meta,
            outgoing: HashMap::new(),
            incoming: HashMap::new(),
        }
    }

    #[inline]
    fn is_isolated(&self, entity: Entity) -> bool {
        !self.outgoing.contains_key(&entity) && !self.incoming.contains_key(&entity)
    }

    #[inline]
    fn has_edge(&self, source: Entity, target: Entity) -> bool {
        self.outgoing
            .get(&source)
            .is_some_and(|edges| edges.iter().any(|(t, _)| *t == target))
    }

    #[inline]
    fn edge_data(&self, source: Entity, target: Entity) -> Option<&[u8]> {
        self.outgoing
            .get(&source)?
            .iter()
            .find(|(t, _)| *t == target)
            .map(|(_, data)| data.as_ref())
    }

    #[inline]
    fn outgoing_len(&self, source: Entity) -> usize {
        self.outgoing.get(&source).map_or(0, Vec::len)
    }

    fn edge_count(&self) -> usize {
        self.outgoing.values().map(Vec::len).sum()
    }

    fn insert_directed_raw(&mut self, source: Entity, target: Entity, payload: Box<[u8]>) {
        let out_vec = self.outgoing.entry(source).or_default();
        if let Some((_, existing_data)) = out_vec.iter_mut().find(|(t, _)| *t == target) {
            *existing_data = payload;
            return;
        }
        out_vec.push((target, payload));
        let in_vec = self.incoming.entry(target).or_default();
        if !in_vec.contains(&source) {
            in_vec.push(source);
        }
    }

    fn remove_directed_raw(&mut self, source: Entity, target: Entity) -> bool {
        let mut removed = false;
        if let Some(out_vec) = self.outgoing.get_mut(&source) {
            let before = out_vec.len();
            out_vec.retain(|(t, _)| *t != target);
            removed = out_vec.len() != before;
            if out_vec.is_empty() {
                self.outgoing.remove(&source);
            }
        }
        if let Some(in_vec) = self.incoming.get_mut(&target) {
            in_vec.retain(|&s| s != source);
            if in_vec.is_empty() {
                self.incoming.remove(&target);
            }
        }
        removed
    }

    /// Removes all outgoing and incoming edges incident to `entity`, returning every distinct
    /// neighbor `n != entity` whose adjacency lists were modified and which is now isolated.
    fn remove_all_for_entity(&mut self, entity: Entity) -> Vec<Entity> {
        let mut touched: Vec<Entity> = Vec::new();

        if let Some(out_edges) = self.outgoing.remove(&entity) {
            for (target, _) in out_edges {
                if target == entity {
                    continue;
                }
                if let Some(in_vec) = self.incoming.get_mut(&target) {
                    in_vec.retain(|&s| s != entity);
                    if in_vec.is_empty() {
                        self.incoming.remove(&target);
                    }
                }
                if !touched.contains(&target) {
                    touched.push(target);
                }
            }
        }

        if let Some(in_sources) = self.incoming.remove(&entity) {
            for source in in_sources {
                if source == entity {
                    continue;
                }
                if let Some(out_vec) = self.outgoing.get_mut(&source) {
                    out_vec.retain(|(t, _)| *t != entity);
                    if out_vec.is_empty() {
                        self.outgoing.remove(&source);
                    }
                }
                if !touched.contains(&source) {
                    touched.push(source);
                }
            }
        }

        touched.retain(|&n| self.is_isolated(n));
        touched
    }

    /// Collects edges that would be evicted by inserting `(source -> target)` under `EXCLUSIVE`
    /// (and `SYMMETRIC` when combined).
    fn exclusive_evictions(&self, source: Entity, target: Entity) -> Vec<(Entity, Entity)> {
        if !self.meta.is_exclusive() {
            return Vec::new();
        }
        let mut evicted = Vec::new();
        if let Some(out_vec) = self.outgoing.get(&source) {
            for &(old_t, _) in out_vec {
                evicted.push((source, old_t));
                if self.meta.is_symmetric() && old_t != source {
                    evicted.push((old_t, source));
                }
            }
        }
        if self.meta.is_symmetric()
            && source != target
            && let Some(out_vec) = self.outgoing.get(&target)
        {
            for &(old_u, _) in out_vec {
                evicted.push((target, old_u));
                if old_u != target {
                    evicted.push((old_u, target));
                }
            }
        }
        evicted
    }

    /// Returns `true` if `goal` is reachable from `start` along outgoing edges while ignoring any
    /// edge in `skip_edges`.
    fn is_reachable_skipping(
        &self,
        start: Entity,
        goal: Entity,
        skip_edges: &[(Entity, Entity)],
    ) -> bool {
        if start == goal {
            return true;
        }
        let mut visited: HashSet<Entity> = HashSet::new();
        let mut queue: VecDeque<Entity> = VecDeque::new();
        visited.insert(start);
        queue.push_back(start);

        while let Some(cur) = queue.pop_front() {
            let Some(neighbors) = self.outgoing.get(&cur) else {
                continue;
            };
            for &(next, _) in neighbors {
                if skip_edges.contains(&(cur, next)) {
                    continue;
                }
                if next == goal {
                    return true;
                }
                if visited.insert(next) {
                    queue.push_back(next);
                }
            }
        }
        false
    }

    /// Checks whether inserting `(source -> target)` preserves acyclicity.
    fn check_acyclic(&self, source: Entity, target: Entity) -> FfiResult {
        if source == target {
            return FfiResult::CYCLE_DETECTED;
        }
        if self.has_edge(source, target) {
            return FfiResult::OK;
        }
        let evicted = self.exclusive_evictions(source, target);
        if self.is_reachable_skipping(target, source, &evicted) {
            return FfiResult::CYCLE_DETECTED;
        }
        FfiResult::OK
    }
}

/// Per-`World` resource storing typed entity relationship edges, payloads, and flush diagnostics.
#[derive(Resource, Default, Debug)]
pub struct ModRelationGraph {
    tables: HashMap<BamComponent, RelationEdgeTable>,
    by_component_id: HashMap<ComponentId, BamComponent>,
    flush_policy: RelationFlushPolicy,
    flush_errors: Vec<RelationFlushError>,
}

/// Ensures the per-`World` [`ModRelationGraph`] resource is initialized.
#[inline]
pub fn ensure_relation_graph_in_world(world: &mut World) {
    if !world.contains_resource::<ModRelationGraph>() {
        world.insert_resource(ModRelationGraph::default());
    }
}

/// Deterministic cycle-detection check for adding directed edge `(source -> target)` to `relation`
/// in `graph`.
///
/// Returns:
/// - [`FfiResult::INVALID_COMPONENT`] if `relation` is not registered in `graph`.
/// - [`FfiResult::CYCLE_DETECTED`] if `source == target` (self-loop) or `source` is reachable from
///   `target` along `relation` (accounting for any `EXCLUSIVE` edge eviction).
/// - [`FfiResult::OK`] if adding (or updating) `(source -> target)` preserves acyclicity.
#[inline]
pub fn check_relation_acyclic(
    graph: &ModRelationGraph,
    relation: BamComponent,
    source: Entity,
    target: Entity,
) -> FfiResult {
    let Some(table) = graph.tables.get(&relation) else {
        return FfiResult::INVALID_COMPONENT;
    };
    table.check_acyclic(source, target)
}

impl ModRelationGraph {
    /// Creates an empty [`ModRelationGraph`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `meta` inside this graph. Idempotent across hot reloads: if an identical
    /// `(handle, uuid, component_id)` table is already present, all existing edges are preserved.
    pub(crate) fn ensure_relation_table(&mut self, meta: RegisteredRelation) {
        let handle = meta.handle;
        let comp_id = meta.component_id;
        if let Some(existing) = self.tables.get_mut(&handle)
            && existing.meta.uuid == meta.uuid
            && existing.meta.component_id == comp_id
        {
            existing.meta = meta;
            self.by_component_id.insert(comp_id, handle);
            return;
        }
        if let Some(prev) = self.tables.insert(handle, RelationEdgeTable::new(meta)) {
            self.by_component_id.remove(&prev.meta.component_id);
        }
        self.by_component_id.insert(comp_id, handle);
    }

    #[inline]
    fn handle_by_component_id(&self, comp_id: ComponentId) -> Option<BamComponent> {
        self.by_component_id.get(&comp_id).copied()
    }

    /// Returns the number of relationships registered in this graph.
    #[inline]
    pub fn relation_count(&self) -> usize {
        self.tables.len()
    }

    /// Borrows the [`RegisteredRelation`] metadata for `relation`, if present.
    #[inline]
    pub fn relation_meta(&self, relation: BamComponent) -> Option<&RegisteredRelation> {
        self.tables.get(&relation).map(|t| &t.meta)
    }

    /// Returns `true` if directed edge `(source -> target)` exists in `relation`.
    #[inline]
    pub fn has_edge(&self, relation: BamComponent, source: Entity, target: Entity) -> bool {
        self.tables
            .get(&relation)
            .is_some_and(|t| t.has_edge(source, target))
    }

    /// Borrows the raw POD payload bytes stored on edge `(source -> target)`, if present.
    #[inline]
    pub fn edge_data(
        &self,
        relation: BamComponent,
        source: Entity,
        target: Entity,
    ) -> Option<&[u8]> {
        self.tables.get(&relation)?.edge_data(source, target)
    }

    /// Returns all outgoing target entities of `source` for `relation` in insertion order.
    pub fn targets(&self, relation: BamComponent, source: Entity) -> Vec<Entity> {
        self.tables
            .get(&relation)
            .and_then(|t| t.outgoing.get(&source))
            .map_or_else(Vec::new, |edges| edges.iter().map(|(t, _)| *t).collect())
    }

    /// Returns all incoming source entities pointing to `target` for `relation`.
    pub fn sources(&self, relation: BamComponent, target: Entity) -> Vec<Entity> {
        self.tables
            .get(&relation)
            .and_then(|t| t.incoming.get(&target))
            .cloned()
            .unwrap_or_default()
    }

    /// Returns read-only `(target, data)` views for all outgoing edges of `source` in `relation`.
    pub fn outgoing_edges(
        &self,
        relation: BamComponent,
        source: Entity,
    ) -> Vec<RelationEdgeView<'_>> {
        self.tables
            .get(&relation)
            .and_then(|t| t.outgoing.get(&source))
            .map_or_else(Vec::new, |edges| {
                edges
                    .iter()
                    .map(|(target, data)| RelationEdgeView {
                        target: *target,
                        data: data.as_ref(),
                    })
                    .collect()
            })
    }

    /// Returns the total number of stored directed edges for `relation`.
    #[inline]
    pub fn edge_count(&self, relation: BamComponent) -> usize {
        self.tables
            .get(&relation)
            .map_or(0, RelationEdgeTable::edge_count)
    }

    /// Returns `true` if `entity` has zero incoming and zero outgoing edges for `relation`.
    #[inline]
    pub fn is_entity_isolated(&self, relation: BamComponent, entity: Entity) -> bool {
        self.tables
            .get(&relation)
            .is_none_or(|t| t.is_isolated(entity))
    }

    /// Returns the active [`RelationFlushPolicy`].
    #[inline]
    pub const fn flush_policy(&self) -> RelationFlushPolicy {
        self.flush_policy
    }

    /// Configures the [`RelationFlushPolicy`] used during deferred command flush.
    #[inline]
    pub fn set_flush_policy(&mut self, policy: RelationFlushPolicy) {
        self.flush_policy = policy;
    }

    /// Borrows all [`RelationFlushError`] diagnostics accumulated during deferred command flushes.
    #[inline]
    pub fn flush_errors(&self) -> &[RelationFlushError] {
        &self.flush_errors
    }

    /// Returns the most recent [`RelationFlushError`], if any.
    #[inline]
    pub fn last_flush_error(&self) -> Option<&RelationFlushError> {
        self.flush_errors.last()
    }

    /// Drains and returns all accumulated [`RelationFlushError`] diagnostics.
    #[inline]
    pub fn take_flush_errors(&mut self) -> Vec<RelationFlushError> {
        core::mem::take(&mut self.flush_errors)
    }

    /// Clears all accumulated [`RelationFlushError`] diagnostics.
    #[inline]
    pub fn clear_flush_errors(&mut self) {
        self.flush_errors.clear();
    }

    fn record_flush_error(&mut self, err: RelationFlushError) {
        match self.flush_policy {
            RelationFlushPolicy::LogAndSkip | RelationFlushPolicy::AbortRemainingRelations => {
                tracing::warn!(
                    relation = err.relation.0,
                    entity = ?err.entity,
                    target = ?err.target,
                    status = %err.status,
                    "deferred relationship command rejected during flush"
                );
            }
            RelationFlushPolicy::RecordOnly => {}
        }
        self.flush_errors.push(err);
    }

    fn on_entity_despawn(
        &mut self,
        handle: BamComponent,
        root: Entity,
    ) -> (Vec<Entity>, Vec<Entity>) {
        let Some(table) = self.tables.get_mut(&handle) else {
            return (Vec::new(), Vec::new());
        };
        if table.is_isolated(root) {
            return (Vec::new(), Vec::new());
        }
        if !table.meta.is_cascade_despawn() {
            let isolated = table.remove_all_for_entity(root);
            return (Vec::new(), isolated);
        }

        // Iterative cycle-safe BFS collecting every dependent source entity transitively pointing
        // into `root` along this `CASCADE_DESPAWN` / `CASCADE_DELETE` relation.
        let mut visited: HashSet<Entity> = HashSet::new();
        let mut queue: VecDeque<Entity> = VecDeque::new();
        let mut victims: Vec<Entity> = Vec::new();
        let mut candidate_neighbors: Vec<Entity> = Vec::new();

        visited.insert(root);
        queue.push_back(root);

        while let Some(cur) = queue.pop_front() {
            if let Some(srcs) = table.incoming.get(&cur) {
                for &src in srcs {
                    if visited.insert(src) {
                        victims.push(src);
                        queue.push_back(src);
                    }
                }
            }
            let touched = table.remove_all_for_entity(cur);
            for neighbor in touched {
                if !visited.contains(&neighbor) {
                    candidate_neighbors.push(neighbor);
                }
            }
        }

        let mut isolated: Vec<Entity> = Vec::new();
        let mut seen_iso: HashSet<Entity> = HashSet::new();
        for neighbor in candidate_neighbors {
            if !visited.contains(&neighbor)
                && table.is_isolated(neighbor)
                && seen_iso.insert(neighbor)
            {
                isolated.push(neighbor);
            }
        }

        (victims, isolated)
    }

    fn on_entity_remove(&mut self, handle: BamComponent, entity: Entity) -> Vec<Entity> {
        let Some(table) = self.tables.get_mut(&handle) else {
            return Vec::new();
        };
        if table.is_isolated(entity) {
            return Vec::new();
        }
        table.remove_all_for_entity(entity)
    }

    /// Synchronously inserts edge `(source -> target)` with `data` into `world`'s
    /// [`ModRelationGraph`], enforcing `EXCLUSIVE`, `max_degree`, `ACYCLIC`, and `SYMMETRIC`
    /// invariants atomically and attaching the relation's Bevy marker component.
    pub fn insert_edge_in_world(
        world: &mut World,
        relation: BamComponent,
        source: Entity,
        target: Entity,
        data: &[u8],
    ) -> FfiResult {
        if !world.entities().contains(source) || !world.entities().contains(target) {
            return FfiResult::INVALID_ENTITY;
        }
        ensure_relation_graph_in_world(world);
        if world
            .resource::<ModRelationGraph>()
            .relation_meta(relation)
            .is_none()
            && let Some(reg_meta) = world
                .get_resource::<ModRegistry>()
                .and_then(|r| r.get_relation(relation).cloned())
        {
            world
                .resource_mut::<ModRelationGraph>()
                .ensure_relation_table(reg_meta);
        }

        let (rel_cid, to_strip) = {
            let mut graph = world.resource_mut::<ModRelationGraph>();
            let Some(table) = graph.tables.get_mut(&relation) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if data.len() != table.meta.data_size {
                return FfiResult::INVALID_ARGUMENT;
            }
            if table.meta.is_acyclic() && source == target {
                return FfiResult::CYCLE_DETECTED;
            }

            // In-place payload update for an already-existing edge preserves degree & acyclicity.
            if table.has_edge(source, target) {
                let payload: Box<[u8]> = Box::from(data);
                table.insert_directed_raw(source, target, payload);
                if table.meta.is_symmetric() && source != target {
                    table.insert_directed_raw(target, source, Box::from(data));
                }
                return FfiResult::OK;
            }

            // Validate constraints atomically BEFORE mutating any adjacency lists.
            if table.meta.is_acyclic() {
                let status = table.check_acyclic(source, target);
                if status.is_err() {
                    return status;
                }
            }
            if !table.meta.is_exclusive()
                && let Some(max_deg) = table.meta.effective_max_degree()
            {
                if table.outgoing_len(source) >= max_deg {
                    return FfiResult::CAPACITY_EXCEEDED;
                }
                if table.meta.is_symmetric()
                    && source != target
                    && table.outgoing_len(target) >= max_deg
                {
                    return FfiResult::CAPACITY_EXCEEDED;
                }
            }

            let mut potentially_isolated: Vec<Entity> = Vec::new();
            if table.meta.is_exclusive() {
                let old_targets: Vec<Entity> = table
                    .outgoing
                    .get(&source)
                    .map(|v| v.iter().map(|(t, _)| *t).collect())
                    .unwrap_or_default();
                for old_t in old_targets {
                    table.remove_directed_raw(source, old_t);
                    if table.meta.is_symmetric() && old_t != source {
                        table.remove_directed_raw(old_t, source);
                    }
                    if old_t != source && old_t != target && !potentially_isolated.contains(&old_t)
                    {
                        potentially_isolated.push(old_t);
                    }
                }
                if table.meta.is_symmetric() && source != target {
                    let old_target_peers: Vec<Entity> = table
                        .outgoing
                        .get(&target)
                        .map(|v| v.iter().map(|(t, _)| *t).collect())
                        .unwrap_or_default();
                    for old_u in old_target_peers {
                        table.remove_directed_raw(target, old_u);
                        if old_u != target {
                            table.remove_directed_raw(old_u, target);
                        }
                        if old_u != source
                            && old_u != target
                            && !potentially_isolated.contains(&old_u)
                        {
                            potentially_isolated.push(old_u);
                        }
                    }
                }
            }

            table.insert_directed_raw(source, target, Box::from(data));
            if table.meta.is_symmetric() && source != target {
                table.insert_directed_raw(target, source, Box::from(data));
            }

            potentially_isolated.retain(|&e| table.is_isolated(e));
            (table.meta.component_id, potentially_isolated)
        };

        ensure_marker_attached(world, source, rel_cid);
        if target != source {
            ensure_marker_attached(world, target, rel_cid);
        }
        strip_isolated_markers(world, rel_cid, &to_strip);
        FfiResult::OK
    }

    /// Synchronously removes edge `(source -> target)` (and reciprocal `(target -> source)` when
    /// `SYMMETRIC`) from `world`'s [`ModRelationGraph`], stripping the Bevy marker component from
    /// any endpoint that becomes isolated. Idempotent when the edge is absent.
    pub fn remove_edge_in_world(
        world: &mut World,
        relation: BamComponent,
        source: Entity,
        target: Entity,
    ) -> FfiResult {
        ensure_relation_graph_in_world(world);
        if world
            .resource::<ModRelationGraph>()
            .relation_meta(relation)
            .is_none()
            && let Some(reg_meta) = world
                .get_resource::<ModRegistry>()
                .and_then(|r| r.get_relation(relation).cloned())
        {
            world
                .resource_mut::<ModRelationGraph>()
                .ensure_relation_table(reg_meta);
        }

        let (rel_cid, to_strip) = {
            let mut graph = world.resource_mut::<ModRelationGraph>();
            let Some(table) = graph.tables.get_mut(&relation) else {
                return FfiResult::INVALID_COMPONENT;
            };
            table.remove_directed_raw(source, target);
            if table.meta.is_symmetric() && source != target {
                table.remove_directed_raw(target, source);
            }
            let mut isolated = Vec::new();
            if table.is_isolated(source) {
                isolated.push(source);
            }
            if target != source && table.is_isolated(target) {
                isolated.push(target);
            }
            (table.meta.component_id, isolated)
        };

        strip_isolated_markers(world, rel_cid, &to_strip);
        FfiResult::OK
    }
}

fn ensure_marker_attached(world: &mut World, entity: Entity, rel_cid: ComponentId) {
    if world.components().get_info(rel_cid).is_none() {
        return;
    }
    if let Ok(mut ent_mut) = world.get_entity_mut(entity)
        && !ent_mut.contains_id(rel_cid)
    {
        // SAFETY: `rel_cid` was registered as a zero-sized `(0, 1)` sparse-set marker component
        // with no drop function, so `NonNull::dangling()` is a valid ZST `OwningPtr`.
        let owning = unsafe { OwningPtr::new(core::ptr::NonNull::<u8>::dangling()) };
        // SAFETY: `rel_cid` was verified registered in `world` above with `(0, 1)` layout.
        unsafe {
            ent_mut.insert_by_id(rel_cid, owning);
        }
    }
}

fn strip_isolated_markers(world: &mut World, rel_cid: ComponentId, candidates: &[Entity]) {
    if candidates.is_empty() || world.components().get_info(rel_cid).is_none() {
        return;
    }
    for &entity in candidates {
        let still_isolated = world
            .get_resource::<ModRelationGraph>()
            .and_then(|g| g.handle_by_component_id(rel_cid).map(|h| (g, h)))
            .is_some_and(|(g, h)| g.is_entity_isolated(h, entity));
        if still_isolated
            && let Ok(mut ent_mut) = world.get_entity_mut(entity)
            && ent_mut.contains_id(rel_cid)
        {
            ent_mut.remove_by_id(rel_cid);
        }
    }
}

fn relation_on_despawn_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    let rel_cid = ctx.component_id;
    let entity = ctx.entity;

    let (cascade_victims, isolated_neighbors) = {
        let Some(mut graph) = world.get_resource_mut::<ModRelationGraph>() else {
            return;
        };
        let Some(handle) = graph.handle_by_component_id(rel_cid) else {
            return;
        };
        graph.on_entity_despawn(handle, entity)
    };

    if cascade_victims.is_empty() && isolated_neighbors.is_empty() {
        return;
    }

    world.commands().queue(move |w: &mut World| {
        for victim in cascade_victims {
            if let Ok(ent_mut) = w.get_entity_mut(victim) {
                ent_mut.despawn();
            }
        }
        strip_isolated_markers(w, rel_cid, &isolated_neighbors);
    });
}

fn relation_on_remove_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    let rel_cid = ctx.component_id;
    let entity = ctx.entity;

    let isolated_neighbors = {
        let Some(mut graph) = world.get_resource_mut::<ModRelationGraph>() else {
            return;
        };
        let Some(handle) = graph.handle_by_component_id(rel_cid) else {
            return;
        };
        graph.on_entity_remove(handle, entity)
    };

    if isolated_neighbors.is_empty() {
        return;
    }

    world.commands().queue(move |w: &mut World| {
        strip_isolated_markers(w, rel_cid, &isolated_neighbors);
    });
}

#[inline]
unsafe fn validate_non_empty_cstr<'a>(ptr: *const c_char) -> Result<&'a str, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: Caller guarantees `ptr` points to a valid NUL-terminated C string.
    let s = unsafe { CStr::from_ptr(ptr) }
        .to_str()
        .map_err(|_| FfiResult::INVALID_UTF8)?;
    if s.is_empty() {
        return Err(FfiResult::INVALID_ARGUMENT);
    }
    Ok(s)
}

fn allocate_standalone_component_id(snapshot: &RegistrySnapshot) -> ComponentId {
    let mut candidate = 1_000_000usize.saturating_add(snapshot.component_count());
    while snapshot.get_by_id(ComponentId::new(candidate)).is_some() {
        candidate = candidate.wrapping_add(1);
    }
    ComponentId::new(candidate)
}

/// Core implementation backing [`crate::host_register_relation`].
///
/// # Safety
/// - `load_ctx` must point to an active `LoadContext` cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamRelationDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub(crate) unsafe fn register_relation_impl(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamRelationDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamRelationDesc::MIN_V0_3_SIZE`.
    let desc = match unsafe { read_descriptor::<BamRelationDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamRelationDesc header ({err})"));
            return err;
        }
    };
    let status = desc.validate();
    if status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamRelationDesc validation failed ({status})"));
        return status;
    }
    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let name_str = match unsafe { validate_non_empty_cstr(desc.name) } {
        Ok(s) => s,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("BamRelationDesc `name` validation failed ({err})"));
            return err;
        }
    };
    if (desc.flags & BAM_RELATION_FLAG_EXCLUSIVE) != 0 && desc.max_degree > 1 {
        ctx.host_ctx.set_last_error(format!(
            "BamRelationDesc '{name_str}' sets EXCLUSIVE with incompatible max_degree={}",
            desc.max_degree
        ));
        return FfiResult::INVALID_ARGUMENT;
    }

    // Prefix unqualified relation names with `"<manifest.name>::<Name>"` unless already present.
    let canonical_name: std::borrow::Cow<'_, str> =
        if !name_str.contains("::") && ctx.registry.get_by_name(name_str).is_none() {
            if let Some(ref ns) = ctx.manifest_name {
                std::borrow::Cow::Owned(format!("{ns}::{name_str}"))
            } else {
                std::borrow::Cow::Borrowed(name_str)
            }
        } else {
            std::borrow::Cow::Borrowed(name_str)
        };
    let name_ref = canonical_name.as_ref();

    let existing_rel = match ctx.registry.check_relation_registration_collision(
        name_ref,
        desc.uuid,
        desc.flags,
        desc.data_size,
        desc.data_align,
        desc.max_degree,
    ) {
        Ok(existing) => existing,
        Err(collision) => {
            ctx.host_ctx.set_last_error(collision.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    let comp_id = if let Some(ref existing) = existing_rel {
        existing.component_id
    } else if !ctx.world_ptr.is_null() {
        // SAFETY: `ctx.world_ptr` comes from an exclusively borrowed `&mut World` valid for the
        // duration of `bevy_abi_mod_init`.
        let world = unsafe { &mut *ctx.world_ptr };
        let zst_layout = Layout::from_size_align(0, 1).expect("valid ZST marker layout");
        // SAFETY: Zero-sized `(0, 1)` sparse-set marker with no drop fn and `Ignore` clone behavior.
        let bevy_desc = unsafe {
            ComponentDescriptor::new_with_layout(
                name_ref.to_string(),
                StorageType::SparseSet,
                zst_layout,
                None,
                false,
                false,
                ComponentCloneBehavior::Ignore,
                None,
            )
        };
        let new_id = world.register_component_with_descriptor(bevy_desc);
        if let Some(hooks) = world.register_component_hooks_by_id(new_id) {
            hooks.on_remove(relation_on_remove_hook);
            hooks.on_despawn(relation_on_despawn_hook);
        }
        new_id
    } else {
        allocate_standalone_component_id(ctx.registry.snapshot())
    };

    let rel_meta = match ctx.registry.register_relation_entry(
        comp_id,
        name_ref,
        desc.uuid,
        desc.flags,
        desc.data_size,
        desc.data_align,
        desc.max_degree,
        ctx.mod_id,
    ) {
        Ok(meta) => meta,
        Err(err) => {
            ctx.host_ctx.set_last_error(err.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    if !ctx.world_ptr.is_null() {
        // SAFETY: `ctx.world_ptr` is exclusively borrowed for the duration of `bevy_abi_mod_init`.
        let world = unsafe { &mut *ctx.world_ptr };
        ensure_relation_graph_in_world(world);
        world
            .resource_mut::<ModRelationGraph>()
            .ensure_relation_table(rel_meta.clone());
        let updated_reg = ctx.registry.clone();
        world.insert_resource(updated_reg);
    }

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.registered_components.contains(&rel_meta.handle) {
        ctx.registered_components.push(rel_meta.handle);
    }

    // SAFETY: `out_component` was verified non-null above and caller guarantees writable storage.
    unsafe {
        *out_component = rel_meta.handle;
    }
    FfiResult::OK
}

/// Core implementation backing [`crate::host_cmd_insert_relation`].
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - When `data_size > 0`, `data` must point to at least `data_size` initialized readable bytes.
pub(crate) unsafe fn cmd_insert_relation_impl(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
    data: *const c_void,
    data_size: usize,
) -> FfiResult {
    if data_size > 0 && data.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    // SAFETY: `cmds` is validated and caller guarantees `data` is readable for `data_size` bytes.
    unsafe { cmds.insert_relation_raw(entity, relation, target, data.cast::<u8>(), data_size) }
}

/// Core implementation backing [`crate::host_cmd_remove_relation`].
///
/// # Safety
/// `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
pub(crate) unsafe fn cmd_remove_relation_impl(
    commands: *mut BamCommands,
    entity: FfiEntity,
    relation: BamComponent,
    target: FfiEntity,
) -> FfiResult {
    // SAFETY: Validating `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let cmds = match unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    cmds.remove_relation_raw(entity, relation, target)
}

/// Applies a deferred `InsertRelation` command during [`crate::HostCommandsStorage::apply_to_world`],
/// recording any [`RelationFlushError`] according to [`ModRelationGraph::flush_policy`].
///
/// Returns `(status, abort_remaining_relations)`.
pub(crate) fn apply_insert_relation(
    world: &mut World,
    snapshot: &RegistrySnapshot,
    entity: Entity,
    relation: BamComponent,
    target: Entity,
    data: &[u8],
) -> (FfiResult, bool) {
    ensure_relation_graph_in_world(world);
    if world
        .resource::<ModRelationGraph>()
        .relation_meta(relation)
        .is_none()
        && let Some(reg_meta) = snapshot.get_relation(relation).cloned()
    {
        world
            .resource_mut::<ModRelationGraph>()
            .ensure_relation_table(reg_meta);
    }

    let status = ModRelationGraph::insert_edge_in_world(world, relation, entity, target, data);
    if status.is_err() {
        let mut graph = world.resource_mut::<ModRelationGraph>();
        let abort = graph.flush_policy() == RelationFlushPolicy::AbortRemainingRelations;
        graph.record_flush_error(RelationFlushError {
            relation,
            entity,
            target,
            status,
        });
        return (status, abort);
    }
    (FfiResult::OK, false)
}

/// Applies a deferred `RemoveRelation` command during [`crate::HostCommandsStorage::apply_to_world`],
/// recording any [`RelationFlushError`] according to [`ModRelationGraph::flush_policy`].
///
/// Returns `(status, abort_remaining_relations)`.
pub(crate) fn apply_remove_relation(
    world: &mut World,
    snapshot: &RegistrySnapshot,
    entity: Entity,
    relation: BamComponent,
    target: Entity,
) -> (FfiResult, bool) {
    ensure_relation_graph_in_world(world);
    if world
        .resource::<ModRelationGraph>()
        .relation_meta(relation)
        .is_none()
        && let Some(reg_meta) = snapshot.get_relation(relation).cloned()
    {
        world
            .resource_mut::<ModRelationGraph>()
            .ensure_relation_table(reg_meta);
    }

    let status = ModRelationGraph::remove_edge_in_world(world, relation, entity, target);
    if status.is_err() {
        let mut graph = world.resource_mut::<ModRelationGraph>();
        let abort = graph.flush_policy() == RelationFlushPolicy::AbortRemainingRelations;
        graph.record_flush_error(RelationFlushError {
            relation,
            entity,
            target,
            status,
        });
        return (status, abort);
    }
    (FfiResult::OK, false)
}

/// Evaluates whether adding `(entity -> target)` to an `ACYCLIC` relation would close a cycle
/// purely within the pending `(is_insert, entity, target)` command sequence.
pub(crate) fn check_pending_acyclic(
    rel: &RegisteredRelation,
    pending_ops: impl Iterator<Item = PendingRelationOp>,
    new_entity: Entity,
    new_target: Entity,
) -> FfiResult {
    if new_entity == new_target {
        return FfiResult::CYCLE_DETECTED;
    }
    let mut table = RelationEdgeTable::new(rel.clone());
    for op in pending_ops {
        match op {
            PendingRelationOp::Insert { entity, target } => {
                if table.meta.is_exclusive() {
                    let old_targets: Vec<Entity> = table
                        .outgoing
                        .get(&entity)
                        .map(|v| v.iter().map(|(t, _)| *t).collect())
                        .unwrap_or_default();
                    for old_t in old_targets {
                        table.remove_directed_raw(entity, old_t);
                        if table.meta.is_symmetric() && old_t != entity {
                            table.remove_directed_raw(old_t, entity);
                        }
                    }
                    if table.meta.is_symmetric() && entity != target {
                        let old_peers: Vec<Entity> = table
                            .outgoing
                            .get(&target)
                            .map(|v| v.iter().map(|(t, _)| *t).collect())
                            .unwrap_or_default();
                        for old_u in old_peers {
                            table.remove_directed_raw(target, old_u);
                            if old_u != target {
                                table.remove_directed_raw(old_u, target);
                            }
                        }
                    }
                }
                table.insert_directed_raw(entity, target, Box::new([]));
                if table.meta.is_symmetric() && entity != target {
                    table.insert_directed_raw(target, entity, Box::new([]));
                }
            }
            PendingRelationOp::Remove { entity, target } => {
                table.remove_directed_raw(entity, target);
                if table.meta.is_symmetric() && entity != target {
                    table.remove_directed_raw(target, entity);
                }
            }
            PendingRelationOp::Despawn { entity } => {
                let _ = table.remove_all_for_entity(entity);
            }
        }
    }
    table.check_acyclic(new_entity, new_target)
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum PendingRelationOp {
    Insert { entity: Entity, target: Entity },
    Remove { entity: Entity, target: Entity },
    Despawn { entity: Entity },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::commands::{HostCommandsStorage, host_cmd_despawn, host_cmd_remove};
    use crate::ffi::register::LoadContext;
    use crate::ffi::v03_ext::{
        host_cmd_checkpoint, host_cmd_insert_relation, host_cmd_remove_relation, host_cmd_rollback,
        host_register_relation,
    };
    use crate::loader::ModId;
    use bevy_abi_mod_sys::{
        BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE,
        BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_CLEANUP_REMOVE,
        BAM_RELATION_FLAG_SYMMETRIC, ComponentUuid,
    };
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
        let mut desc =
            BamRelationDesc::new(cname.as_ptr(), ComponentUuid::from_canonical_name(&full))
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
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                pair_rel,
                a,
                b,
                &10u32.to_le_bytes()
            ),
            FfiResult::OK
        );
        assert_eq!(
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                pair_rel,
                c,
                d,
                &20u32.to_le_bytes()
            ),
            FfiResult::OK
        );
        // Update payload from reverse direction (B -> A).
        assert_eq!(
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                pair_rel,
                b,
                a,
                &15u32.to_le_bytes()
            ),
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
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                pair_rel,
                a,
                c,
                &30u32.to_le_bytes()
            ),
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
            ModRelationGraph::insert_edge_in_world(
                &mut world,
                BamComponent::from_raw(999),
                a,
                b,
                &[]
            ),
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
            .register_relation_entry(
                collide_cid,
                "collide::Comp",
                ComponentUuid::from_canonical_name("collide::Comp"),
                0,
                0,
                1,
                0,
                None,
            )
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
}
