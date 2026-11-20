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

use crate::ffi::register::validate_active_load_ctx;
use crate::handle_header::{ffi_try, validate_commands_mut, validate_non_empty_cstr};
use crate::registry::RelationRegSpec;
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
use core::ffi::c_void;
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

    /// Evicts existing outgoing edges under `EXCLUSIVE` (and reciprocal `SYMMETRIC` peers),
    /// returning distinct displaced neighbors (`!= source`, `!= target`) that may now be isolated.
    fn evict_exclusive_edges(&mut self, source: Entity, target: Entity) -> Vec<Entity> {
        if !self.meta.is_exclusive() {
            return Vec::new();
        }
        let mut potentially_isolated = Vec::new();
        let old_targets: Vec<Entity> = self
            .outgoing
            .get(&source)
            .map(|v| v.iter().map(|(t, _)| *t).collect())
            .unwrap_or_default();
        for old_t in old_targets {
            self.remove_directed_raw(source, old_t);
            if self.meta.is_symmetric() && old_t != source {
                self.remove_directed_raw(old_t, source);
            }
            if old_t != source && old_t != target && !potentially_isolated.contains(&old_t) {
                potentially_isolated.push(old_t);
            }
        }
        if self.meta.is_symmetric() && source != target {
            let old_target_peers: Vec<Entity> = self
                .outgoing
                .get(&target)
                .map(|v| v.iter().map(|(t, _)| *t).collect())
                .unwrap_or_default();
            for old_u in old_target_peers {
                self.remove_directed_raw(target, old_u);
                if old_u != target {
                    self.remove_directed_raw(old_u, target);
                }
                if old_u != source && old_u != target && !potentially_isolated.contains(&old_u) {
                    potentially_isolated.push(old_u);
                }
            }
        }
        potentially_isolated
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
    world.init_resource::<ModRelationGraph>();
}

fn ensure_relation_table_in_world(
    world: &mut World,
    snapshot: Option<&RegistrySnapshot>,
    relation: BamComponent,
) {
    ensure_relation_graph_in_world(world);
    if world
        .resource::<ModRelationGraph>()
        .relation_meta(relation)
        .is_some()
    {
        return;
    }
    let reg_meta = snapshot
        .and_then(|snap| snap.get_relation(relation).cloned())
        .or_else(|| {
            world
                .get_resource::<ModRegistry>()
                .and_then(|r| r.get_relation(relation).cloned())
        });
    if let Some(meta) = reg_meta {
        world
            .resource_mut::<ModRelationGraph>()
            .ensure_relation_table(meta);
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
        ensure_relation_table_in_world(world, None, relation);

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
                table.insert_directed_raw(source, target, Box::from(data));
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

            let mut potentially_isolated = table.evict_exclusive_edges(source, target);

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
        ensure_relation_table_in_world(world, None, relation);

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
    let ctx = ffi_try!(unsafe { validate_active_load_ctx(load_ctx) });
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
        // SAFETY: Zero-sized `(0, 1)` sparse-set marker with no drop fn and `Ignore` clone behavior.
        let bevy_desc = unsafe {
            ComponentDescriptor::new_with_layout(
                name_ref.to_string(),
                StorageType::SparseSet,
                Layout::new::<()>(),
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

    let rel_meta = match ctx.registry.register_relation_entry(RelationRegSpec {
        comp_id,
        name: name_ref,
        uuid: desc.uuid,
        flags: desc.flags,
        data_size: desc.data_size,
        data_align: desc.data_align,
        max_degree: desc.max_degree,
        owner: ctx.mod_id,
    }) {
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
    let cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
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
    let cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    cmds.remove_relation_raw(entity, relation, target)
}

fn apply_relation_op(
    world: &mut World,
    snapshot: &RegistrySnapshot,
    entity: Entity,
    relation: BamComponent,
    target: Entity,
    op: impl FnOnce(&mut World) -> FfiResult,
) -> (FfiResult, bool) {
    ensure_relation_table_in_world(world, Some(snapshot), relation);
    let status = op(world);
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
    apply_relation_op(world, snapshot, entity, relation, target, |w| {
        ModRelationGraph::insert_edge_in_world(w, relation, entity, target, data)
    })
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
    apply_relation_op(world, snapshot, entity, relation, target, |w| {
        ModRelationGraph::remove_edge_in_world(w, relation, entity, target)
    })
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
                let _ = table.evict_exclusive_edges(entity, target);
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
#[path = "../../tests/unit/relations_tests.rs"]
mod tests;
