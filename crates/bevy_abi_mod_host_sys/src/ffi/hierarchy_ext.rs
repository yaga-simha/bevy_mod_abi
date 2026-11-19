//! Hierarchy inspection (`sys_entity_children`), lazy topological depth-stratum cascade caching
//! ([`CascadeHierarchyCache`]), and cycle-guarded upward ancestor cascade lookup
//! (`host_iter_up_cascade`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{
    BAM_CHILDREN_NAME, BamComponent, BamIter, BamSystemCtx, FfiEntity, FfiResult,
};
use bevy_ecs::{
    archetype::{ArchetypeGeneration, ArchetypeId},
    change_detection::Tick,
    component::{ComponentId, StorageType},
    entity::{Entities, Entity, EntityIndex},
    hierarchy::{ChildOf, Children},
    lifecycle::RemovedComponentEntity,
    message::MessageCursor,
    prelude::Resource,
    storage::{TableId, TableRow},
    world::{World, unsafe_world_cell::UnsafeWorldCell},
};
use bevy_platform::collections::HashMap;
use bevy_tasks::{ComputeTaskPool, TaskPool};
use nonmax::NonMaxU32;

use crate::{
    ffi::iter::HostIter,
    ffi::register::DynamicComponentHookMap,
    handle_header::{ITER_MAGIC, SYS_CTX_MAGIC, validate_handle_ref},
    registry::ModRegistry,
    system::HostSystemCtx,
};

/// Ensures Bevy's built-in [`Children`] hierarchy component is registered in `world` and in its
/// [`ModRegistry`] resource (if present) for zero-copy `sys_entity_children` traversal.
pub fn ensure_children_registered_in_world(world: &mut World) {
    let children_id = world.register_component::<Children>();
    if let Some(mut reg) = world.get_resource_mut::<ModRegistry>()
        && reg.get_by_name(BAM_CHILDREN_NAME).is_none()
    {
        let _ = reg.register_children_hierarchy(children_id);
    }
}

/// Borrows the contiguous slice of child or custom `RelationshipTarget` entities for `entity`,
/// writing the zero-copy pointer and length into `*out_entities` and `*out_len`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - `out_entities` must point to writable `*const FfiEntity` storage, or null.
/// - `out_len` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_sys_entity_children(
    sys_ctx: *const BamSystemCtx,
    entity: FfiEntity,
    relationship_target: BamComponent,
    out_entities: *mut *const FfiEntity,
    out_len: *mut usize,
) -> FfiResult {
    if out_entities.is_null() || out_len.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_entities.is_aligned() || !out_len.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
    unsafe {
        *out_entities = core::ptr::null();
        *out_len = 0;
    }
    // SAFETY: Validates `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return if sys_ctx.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INVALID_ENTITY;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INVALID_ENTITY;
    }

    let (snapshot, world_opt) = ctx.snapshot_and_world();
    let Some(world) = world_opt else {
        return FfiResult::INVALID_STATE;
    };

    let target_meta = if !relationship_target.is_valid() {
        let Some(meta) = snapshot
            .get_by_name(BAM_CHILDREN_NAME)
            .or_else(|| snapshot.get_by_name("Children"))
        else {
            return FfiResult::NOT_FOUND;
        };
        meta
    } else {
        let Some(meta) = snapshot.get_by_handle(relationship_target) else {
            return FfiResult::INVALID_COMPONENT;
        };
        meta
    };

    if !ctx.has_component_read_access(target_meta.component_id) {
        return FfiResult::PERMISSION_DENIED;
    }

    let Ok(entity_cell) = world.get_entity(bevy_entity) else {
        return FfiResult::INVALID_ENTITY;
    };

    if let Some(extract_fn) = target_meta.extract_relationship_targets_fn {
        // SAFETY: `ctx.has_component_read_access` verified declared entity-column read access on
        // `target_meta.component_id` in Bevy's `FilteredAccessSet` for this system execution.
        let Some(comp_ptr) = (unsafe { entity_cell.get_by_id(target_meta.component_id) }) else {
            return FfiResult::OK;
        };
        // SAFETY: `comp_ptr` points to an initialized `RelationshipTarget` component matching
        // `target_meta.component_id` with shared read access for the duration of this system tick.
        let (slice_ptr, slice_len) = unsafe { extract_fn(comp_ptr.as_ptr()) };
        if slice_len > 0 && !slice_ptr.is_null() {
            // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
            unsafe {
                *out_entities = slice_ptr;
                *out_len = slice_len;
            }
        }
        return FfiResult::OK;
    }

    if target_meta.is_dynamic {
        // SAFETY: `DynamicComponentHookMap` is never mutably borrowed concurrently with system
        // execution (only mutated during `LoadContext` or exclusive `DeferredWorld` hooks).
        let Some(hook_map) = (unsafe { world.get_resource::<DynamicComponentHookMap>() }) else {
            return FfiResult::INVALID_COMPONENT;
        };
        if !hook_map.is_relationship_target(target_meta.component_id) {
            return FfiResult::INVALID_COMPONENT;
        }
        let slice = hook_map.relationship_targets_for(target_meta.component_id, bevy_entity);
        if !slice.is_empty() {
            // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
            unsafe {
                *out_entities = slice.as_ptr();
                *out_len = slice.len();
            }
        }
        return FfiResult::OK;
    }

    FfiResult::INVALID_COMPONENT
}

// ============================================================================
// Lazy Per-(World, RelationComponentId) Depth-Stratum Cascade Cache
// ============================================================================

/// Wire layout classification for an entity-valued parent relationship component.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RelationLayout {
    /// Bevy's built-in `ChildOf(Entity)` hierarchy component.
    ChildOf,
    /// Custom component whose leading 8 bytes store an `Entity` / `FfiEntity` parent handle
    /// (including `FfiChildOf`, custom `Entity` wrappers, payload-bearing edges, and
    /// `DynamicComponentHookMap` source components).
    Entity8,
    /// Custom 4-byte component storing a compact `EntityIndex` (`u32`) parent index.
    Entity4,
    /// Zero-sized `DynamicComponentHookMap` relationship target component (`parent -> [children]`).
    DynamicTarget,
}

#[inline]
fn classify_relation_layout(
    world: UnsafeWorldCell<'_>,
    relation_id: ComponentId,
    size: usize,
) -> Option<RelationLayout> {
    if world.components().component_id::<ChildOf>() == Some(relation_id) {
        return Some(RelationLayout::ChildOf);
    }
    if size >= core::mem::size_of::<FfiEntity>() {
        return Some(RelationLayout::Entity8);
    }
    if size == core::mem::size_of::<u32>() {
        return Some(RelationLayout::Entity4);
    }
    if size == 0 {
        // SAFETY: `DynamicComponentHookMap` is never mutably borrowed concurrently with system
        // execution or shared `&World` inspection.
        let is_dyn_target = unsafe { world.get_resource::<DynamicComponentHookMap>() }
            .is_some_and(|hm| hm.is_relationship_target(relation_id));
        if is_dyn_target {
            return Some(RelationLayout::DynamicTarget);
        }
    }
    None
}

/// Extracts a validated live parent [`Entity`] from raw component bytes according to `layout`.
///
/// # Safety
/// - `ptr` must point to at least `size_of::<ChildOf>()` bytes for [`RelationLayout::ChildOf`],
///   8 initialized bytes for [`RelationLayout::Entity8`], or 4 initialized bytes for
///   [`RelationLayout::Entity4`].
#[inline]
unsafe fn extract_parent_from_raw(
    layout: RelationLayout,
    ptr: *const u8,
    entities: &Entities,
) -> Option<Entity> {
    match layout {
        RelationLayout::ChildOf => {
            // SAFETY: Caller guarantees `ptr` points to an initialized `ChildOf` component.
            let parent = unsafe { core::ptr::read_unaligned(ptr.cast::<ChildOf>()) }.parent();
            (parent != Entity::PLACEHOLDER && entities.contains_spawned(parent)).then_some(parent)
        }
        RelationLayout::Entity8 => {
            // SAFETY: Caller guarantees `ptr` points to at least 8 initialized component bytes.
            let bits = unsafe { core::ptr::read_unaligned(ptr.cast::<u64>()) };
            let ffi = FfiEntity::from_bits(bits);
            if ffi.is_null() {
                return None;
            }
            Entity::try_from_bits(bits)
                .filter(|&p| p != Entity::PLACEHOLDER && entities.contains_spawned(p))
        }
        RelationLayout::Entity4 => {
            // SAFETY: Caller guarantees `ptr` points to at least 4 initialized component bytes.
            let raw_idx = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) };
            let idx = EntityIndex::from_raw_u32(raw_idx)?;
            entities
                .is_index_spawned(idx)
                .then(|| entities.resolve_from_index(idx))
        }
        RelationLayout::DynamicTarget => None,
    }
}

/// Mixes a `(parent, child)` edge into a 64-bit order-independent fingerprint for dynamic target
/// change detection.
#[inline]
fn mix_edge_hash(parent: Entity, child: FfiEntity) -> u64 {
    let mut x = parent
        .to_bits()
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(child.to_bits().rotate_left(17));
    x ^= x >> 30;
    x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

struct CachedRelationHierarchy {
    built: bool,
    last_checked_tick: Tick,
    archetype_gen: ArchetypeGeneration,
    removed_cursor: MessageCursor<RemovedComponentEntity>,
    matched_tables: Vec<TableId>,
    matched_archetypes: Vec<ArchetypeId>,
    cached_edge_count: usize,
    cached_dynamic_hash: u64,
    rebuild_count: u64,
    has_cycles: bool,
    parent_map: HashMap<Entity, Entity>,
    depth_map: HashMap<Entity, usize>,
    strata: Vec<Vec<Entity>>,
}

impl Default for CachedRelationHierarchy {
    fn default() -> Self {
        Self {
            built: false,
            last_checked_tick: Tick::new(0),
            archetype_gen: ArchetypeGeneration::initial(),
            removed_cursor: MessageCursor::default(),
            matched_tables: Vec::new(),
            matched_archetypes: Vec::new(),
            cached_edge_count: 0,
            cached_dynamic_hash: 0,
            rebuild_count: 0,
            has_cycles: false,
            parent_map: HashMap::new(),
            depth_map: HashMap::new(),
            strata: Vec::new(),
        }
    }
}

impl CachedRelationHierarchy {
    fn refresh_matches(&mut self, world: &World, relation: ComponentId, storage_type: StorageType) {
        let current_gen = world.archetypes().generation();
        if self.built && self.archetype_gen == current_gen {
            return;
        }
        self.archetype_gen = current_gen;
        match storage_type {
            StorageType::Table => {
                self.matched_tables.clear();
                for arch in world.archetypes().iter() {
                    if arch.contains(relation) {
                        let tid = arch.table_id();
                        if !self.matched_tables.contains(&tid) {
                            self.matched_tables.push(tid);
                        }
                    }
                }
            }
            StorageType::SparseSet => {
                self.matched_archetypes.clear();
                for arch in world.archetypes().iter() {
                    if arch.contains(relation) {
                        self.matched_archetypes.push(arch.id());
                    }
                }
            }
        }
    }

    fn compute_dynamic_target_fingerprint(
        &self,
        world: &World,
        relation: ComponentId,
        storage_type: StorageType,
    ) -> (usize, u64) {
        let Some(hook_map) = world.get_resource::<DynamicComponentHookMap>() else {
            return (0, 0);
        };
        let mut count = 0usize;
        let mut hash = 0u64;
        match storage_type {
            StorageType::Table => {
                let tables = &world.storages().tables;
                for &tid in &self.matched_tables {
                    let Some(table) = tables.get(tid) else {
                        continue;
                    };
                    for &parent in table.entities() {
                        let children = hook_map.relationship_targets_for(relation, parent);
                        count += children.len();
                        for &child in children {
                            hash = hash.wrapping_add(mix_edge_hash(parent, child));
                        }
                    }
                }
            }
            StorageType::SparseSet => {
                let archetypes = world.archetypes();
                for &aid in &self.matched_archetypes {
                    let Some(arch) = archetypes.get(aid) else {
                        continue;
                    };
                    for arch_ent in arch.entities() {
                        let parent = arch_ent.id();
                        let children = hook_map.relationship_targets_for(relation, parent);
                        count += children.len();
                        for &child in children {
                            hash = hash.wrapping_add(mix_edge_hash(parent, child));
                        }
                    }
                }
            }
        }
        (count, hash)
    }

    fn needs_rebuild(
        &mut self,
        world: &World,
        relation: ComponentId,
        storage_type: StorageType,
        layout: RelationLayout,
        this_run: Tick,
    ) -> bool {
        if !self.built {
            return true;
        }

        // 1. Check `RemovedComponents` stream for `relation`.
        let mut dirty = false;
        if let Some(removed_msgs) = world.removed_components().get(relation)
            && self.removed_cursor.read(removed_msgs).count() > 0
        {
            dirty = true;
        }

        self.refresh_matches(world, relation, storage_type);
        if dirty {
            return true;
        }

        // 2. Verify stratum-0 roots remain alive (catches root despawn on custom parent relations
        //    without Bevy `Children` despawn hooks).
        if let Some(roots) = self.strata.first()
            && roots
                .iter()
                .any(|&root| !world.entities().contains_spawned(root))
        {
            return true;
        }

        if layout == RelationLayout::DynamicTarget {
            let (count, hash) =
                self.compute_dynamic_target_fingerprint(world, relation, storage_type);
            return count != self.cached_edge_count || hash != self.cached_dynamic_hash;
        }

        // 3. Check `Changed` ticks and live entity counts across matching storages.
        match storage_type {
            StorageType::Table => {
                let tables = &world.storages().tables;
                let mut total_rows = 0usize;
                for &tid in &self.matched_tables {
                    if let Some(table) = tables.get(tid) {
                        total_rows += table.entity_count() as usize;
                    }
                }
                if total_rows != self.cached_edge_count {
                    return true;
                }
                for &tid in &self.matched_tables {
                    let Some(table) = tables.get(tid) else {
                        continue;
                    };
                    let table_len = table.entity_count() as usize;
                    if table_len == 0 {
                        continue;
                    }
                    let Some(col) = table.get_column(relation) else {
                        continue;
                    };
                    if let Some(summary) = col.get_summary_tick() {
                        if summary
                            .get()
                            .is_newer_than(self.last_checked_tick, this_run)
                        {
                            return true;
                        }
                        continue;
                    }
                    // SAFETY: `table_len == table.entity_count() as usize`.
                    let changed_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                    for tick_cell in changed_ticks {
                        // SAFETY: Shared `&World` borrow guarantees no concurrent tick writes.
                        let tick = unsafe { *tick_cell.get() };
                        if tick.is_newer_than(self.last_checked_tick, this_run) {
                            return true;
                        }
                    }
                }
                false
            }
            StorageType::SparseSet => {
                let Some(ss) = world.storages().sparse_sets.get(relation) else {
                    return self.cached_edge_count != 0;
                };
                if ss.len() != self.cached_edge_count {
                    return true;
                }
                let archetypes = world.archetypes();
                for &aid in &self.matched_archetypes {
                    let Some(arch) = archetypes.get(aid) else {
                        continue;
                    };
                    for arch_ent in arch.entities() {
                        if let Some(tick_cell) = ss.get_changed_tick(arch_ent.id()) {
                            // SAFETY: Shared `&World` borrow guarantees no concurrent tick writes.
                            let tick = unsafe { *tick_cell.get() };
                            if tick.is_newer_than(self.last_checked_tick, this_run) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
        }
    }

    fn rebuild(
        &mut self,
        world: &World,
        relation: ComponentId,
        storage_type: StorageType,
        layout: RelationLayout,
        this_run: Tick,
    ) {
        // Drain any remaining removal records so subsequent unmodified ticks stay clean.
        if let Some(removed_msgs) = world.removed_components().get(relation) {
            let _ = self.removed_cursor.read(removed_msgs).count();
        }
        self.refresh_matches(world, relation, storage_type);

        self.parent_map.clear();
        self.depth_map.clear();
        for stratum in &mut self.strata {
            stratum.clear();
        }
        self.has_cycles = false;
        self.cached_edge_count = 0;
        self.cached_dynamic_hash = 0;

        let entities = world.entities();
        let mut seeds: Vec<Entity> = Vec::new();

        match layout {
            RelationLayout::DynamicTarget => {
                if let Some(hook_map) = world.get_resource::<DynamicComponentHookMap>() {
                    let mut edge_count = 0usize;
                    let mut edge_hash = 0u64;
                    let mut collect_target = |parent: Entity| {
                        let children = hook_map.relationship_targets_for(relation, parent);
                        edge_count += children.len();
                        for &child_ffi in children {
                            edge_hash = edge_hash.wrapping_add(mix_edge_hash(parent, child_ffi));
                            let Some(child) = (!child_ffi.is_null())
                                .then(|| Entity::try_from_bits(child_ffi.to_bits()))
                                .flatten()
                                .filter(|&c| {
                                    c != Entity::PLACEHOLDER && entities.contains_spawned(c)
                                })
                            else {
                                continue;
                            };
                            seeds.push(child);
                            if child == parent {
                                self.has_cycles = true;
                            } else {
                                self.parent_map.insert(child, parent);
                            }
                        }
                    };
                    match storage_type {
                        StorageType::Table => {
                            let tables = &world.storages().tables;
                            for &tid in &self.matched_tables {
                                if let Some(table) = tables.get(tid) {
                                    for &parent in table.entities() {
                                        collect_target(parent);
                                    }
                                }
                            }
                        }
                        StorageType::SparseSet => {
                            let archetypes = world.archetypes();
                            for &aid in &self.matched_archetypes {
                                if let Some(arch) = archetypes.get(aid) {
                                    for arch_ent in arch.entities() {
                                        collect_target(arch_ent.id());
                                    }
                                }
                            }
                        }
                    }
                    self.cached_edge_count = edge_count;
                    self.cached_dynamic_hash = edge_hash;
                }
            }
            RelationLayout::ChildOf | RelationLayout::Entity8 | RelationLayout::Entity4 => {
                match storage_type {
                    StorageType::Table => {
                        let tables = &world.storages().tables;
                        let mut total_rows = 0usize;
                        for &tid in &self.matched_tables {
                            if let Some(table) = tables.get(tid) {
                                total_rows += table.entity_count() as usize;
                            }
                        }
                        self.cached_edge_count = total_rows;
                        seeds.reserve(total_rows);
                        self.parent_map.reserve(total_rows);
                        for &tid in &self.matched_tables {
                            let Some(table) = tables.get(tid) else {
                                continue;
                            };
                            let table_len = table.entity_count() as usize;
                            if table_len == 0 {
                                continue;
                            }
                            let Some(col) = table.get_column(relation) else {
                                continue;
                            };
                            let table_entities = table.entities();
                            for (row_idx, &child) in
                                table_entities.iter().take(table_len).enumerate()
                            {
                                let Some(nonmax) = NonMaxU32::new(row_idx as u32) else {
                                    break;
                                };
                                // SAFETY: `row_idx < table_len == table.entity_count() as usize`.
                                let ptr = unsafe { col.get_data_unchecked(TableRow::new(nonmax)) };
                                // SAFETY: `ptr` points to initialized `relation` bytes matching `layout`.
                                let parent_opt = unsafe {
                                    extract_parent_from_raw(layout, ptr.as_ptr(), entities)
                                };
                                seeds.push(child);
                                if let Some(parent) = parent_opt {
                                    if parent == child {
                                        self.has_cycles = true;
                                    } else {
                                        self.parent_map.insert(child, parent);
                                    }
                                }
                            }
                        }
                    }
                    StorageType::SparseSet => {
                        if let Some(ss) = world.storages().sparse_sets.get(relation) {
                            let total = ss.len();
                            self.cached_edge_count = total;
                            seeds.reserve(total);
                            self.parent_map.reserve(total);
                            let archetypes = world.archetypes();
                            for &aid in &self.matched_archetypes {
                                let Some(arch) = archetypes.get(aid) else {
                                    continue;
                                };
                                for arch_ent in arch.entities() {
                                    let child = arch_ent.id();
                                    let Some(ptr) = ss.get(child) else {
                                        continue;
                                    };
                                    // SAFETY: `ptr` points to initialized `relation` bytes matching `layout`.
                                    let parent_opt = unsafe {
                                        extract_parent_from_raw(layout, ptr.as_ptr(), entities)
                                    };
                                    seeds.push(child);
                                    if let Some(parent) = parent_opt {
                                        if parent == child {
                                            self.has_cycles = true;
                                        } else {
                                            self.parent_map.insert(child, parent);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        // Compute topological depths iteratively (stack-safe for arbitrary chain lengths and
        // cycle-guarded against malformed parent graphs).
        const VISITING: usize = usize::MAX;
        self.depth_map.reserve(seeds.len() + 16);
        let mut all_nodes: Vec<Entity> = Vec::with_capacity(seeds.len() + 16);
        let mut path: Vec<Entity> = Vec::new();
        let mut max_depth: usize = 0;

        for &start in &seeds {
            if self.depth_map.contains_key(&start) {
                continue;
            }
            path.clear();
            let mut curr = start;
            let base_depth = loop {
                match self.depth_map.get(&curr).copied() {
                    Some(VISITING) => {
                        self.has_cycles = true;
                        // Break the cycle edge deterministically right before `curr`, assigning
                        // depth 0 to the tail of `path` and incrementing back along the path.
                        break 0usize;
                    }
                    Some(known_depth) => {
                        break known_depth + 1;
                    }
                    None => {
                        self.depth_map.insert(curr, VISITING);
                        all_nodes.push(curr);
                        path.push(curr);
                        match self.parent_map.get(&curr).copied() {
                            Some(parent) => {
                                curr = parent;
                            }
                            None => {
                                break 0usize;
                            }
                        }
                    }
                }
            };

            for (d, &node) in (base_depth..).zip(path.iter().rev()) {
                self.depth_map.insert(node, d);
                if d > max_depth {
                    max_depth = d;
                }
            }
        }

        if all_nodes.is_empty() {
            self.strata.clear();
        } else {
            let needed = max_depth + 1;
            if self.strata.len() < needed {
                self.strata.resize_with(needed, Vec::new);
            } else {
                self.strata.truncate(needed);
            }
            for &node in &all_nodes {
                if let Some(&depth) = self.depth_map.get(&node) {
                    self.strata[depth].push(node);
                }
            }
        }

        self.built = true;
        self.last_checked_tick = this_run;
        self.rebuild_count += 1;
    }
}

/// Lazy per-`(World, RelationComponentId)` topological depth-stratum cache for breadth-first
/// hierarchy cascades and ancestor lookups.
///
/// Partitions entities participating in a parent relationship component (`ChildOf`, custom
/// 4-byte/8-byte entity-valued parent components, or `DynamicComponentHookMap` relationships) into
/// depth strata `stratum[0..=D_max]` where stratum `0` contains root entities and every child lies
/// strictly at `parent_depth + 1`.
///
/// Rebuilds are triggered lazily by [`Self::ensure_built`] **only** when Bevy change detection
/// (`Changed` ticks or `RemovedComponents`) or root despawn invalidates that specific relation
/// component, guaranteeing zero rebuild overhead on steady-state unmodified hierarchies.
#[derive(Resource, Default)]
pub struct CascadeHierarchyCache {
    by_relation: HashMap<ComponentId, CachedRelationHierarchy>,
}

impl CascadeHierarchyCache {
    /// Creates an empty [`CascadeHierarchyCache`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Ensures the topological depth strata and parent lookup map for `relation` are synchronized
    /// with `world`, rebuilding **only** when `Changed` or `RemovedComponents` fired on `relation`
    /// (or on first build).
    ///
    /// Returns `true` if the cache was freshly built or rebuilt, or `false` if the cached strata
    /// were reused without rebuilding.
    pub fn ensure_built(&mut self, world: &World, relation: ComponentId) -> bool {
        let this_run = world
            .as_unsafe_world_cell_readonly()
            .increment_change_tick();
        let entry = self.by_relation.entry(relation).or_default();

        let Some(comp_info) = world.components().get_info(relation) else {
            if entry.built && entry.strata.is_empty() {
                entry.last_checked_tick = this_run;
                return false;
            }
            entry.parent_map.clear();
            entry.depth_map.clear();
            entry.strata.clear();
            entry.cached_edge_count = 0;
            entry.cached_dynamic_hash = 0;
            entry.has_cycles = false;
            entry.built = true;
            entry.last_checked_tick = this_run;
            entry.rebuild_count += 1;
            return true;
        };

        let storage_type = comp_info.storage_type();
        let Some(layout) = classify_relation_layout(
            world.as_unsafe_world_cell_readonly(),
            relation,
            comp_info.layout().size(),
        ) else {
            if entry.built && entry.strata.is_empty() {
                entry.last_checked_tick = this_run;
                return false;
            }
            entry.parent_map.clear();
            entry.depth_map.clear();
            entry.strata.clear();
            entry.cached_edge_count = 0;
            entry.cached_dynamic_hash = 0;
            entry.has_cycles = false;
            entry.built = true;
            entry.last_checked_tick = this_run;
            entry.rebuild_count += 1;
            return true;
        };

        if !entry.needs_rebuild(world, relation, storage_type, layout, this_run) {
            entry.last_checked_tick = this_run;
            return false;
        }

        entry.rebuild(world, relation, storage_type, layout, this_run);
        true
    }

    /// Convenience helper resolving Bevy's built-in [`ChildOf`] `ComponentId` and ensuring its
    /// depth strata are built. Returns `None` if `ChildOf` is not yet registered in `world`.
    pub fn ensure_built_child_of(&mut self, world: &World) -> Option<(ComponentId, bool)> {
        let cid = world.components().component_id::<ChildOf>()?;
        let rebuilt = self.ensure_built(world, cid);
        Some((cid, rebuilt))
    }

    /// Initializes [`CascadeHierarchyCache`] as a [`World`] resource if absent and runs
    /// [`Self::ensure_built`] for `relation`, returning `true` if rebuilt.
    pub fn ensure_built_in_world(world: &mut World, relation: ComponentId) -> bool {
        if !world.contains_resource::<Self>() {
            world.insert_resource(Self::new());
        }
        world.resource_scope(
            |w: &mut World, mut cache: bevy_ecs::prelude::Mut<'_, Self>| {
                cache.ensure_built(w, relation)
            },
        )
    }

    /// Returns the cached topological depth strata `stratum[0..=D_max]` for `relation`.
    ///
    /// Stratum `0` contains root entities; stratum `d` contains entities at topological depth `d`
    /// (`parent_depth + 1`).
    #[inline]
    pub fn strata(&self, relation: ComponentId) -> &[Vec<Entity>] {
        self.by_relation
            .get(&relation)
            .map_or(&[], |entry| entry.strata.as_slice())
    }

    /// Returns the cached direct live parent of `entity` under `relation`, if any.
    #[inline]
    pub fn parent_of(&self, relation: ComponentId, entity: Entity) -> Option<Entity> {
        self.by_relation
            .get(&relation)
            .and_then(|entry| entry.parent_map.get(&entity).copied())
    }

    /// Returns the cached topological depth index of `entity` under `relation`, if present.
    #[inline]
    pub fn depth_of(&self, relation: ComponentId, entity: Entity) -> Option<usize> {
        self.by_relation
            .get(&relation)
            .and_then(|entry| entry.depth_map.get(&entity).copied())
    }

    /// Returns the cumulative number of builds/rebuilds performed for `relation`.
    #[inline]
    pub fn rebuild_count(&self, relation: ComponentId) -> u64 {
        self.by_relation
            .get(&relation)
            .map_or(0, |entry| entry.rebuild_count)
    }

    /// Returns `true` if the most recent build for `relation` detected and broke at least one cycle.
    #[inline]
    pub fn has_cycles(&self, relation: ComponentId) -> bool {
        self.by_relation
            .get(&relation)
            .is_some_and(|entry| entry.has_cycles)
    }

    /// Explicitly marks `relation`'s cached strata dirty so the next [`Self::ensure_built`]
    /// performs a full rebuild.
    #[inline]
    pub fn invalidate(&mut self, relation: ComponentId) {
        if let Some(entry) = self.by_relation.get_mut(&relation) {
            entry.built = false;
        }
    }

    /// Clears all cached relation hierarchies.
    #[inline]
    pub fn clear(&mut self) {
        self.by_relation.clear();
    }

    /// Single-pass breadth-first cascade helper invoking `f(depth, entity, parent)` in strict
    /// parent-before-child order across `stratum[0..=D_max]`.
    pub fn for_each_cascade(
        &self,
        relation: ComponentId,
        mut f: impl FnMut(usize, Entity, Option<Entity>),
    ) {
        let Some(entry) = self.by_relation.get(&relation) else {
            return;
        };
        for (depth, stratum) in entry.strata.iter().enumerate() {
            for &entity in stratum {
                let parent = entry.parent_map.get(&entity).copied();
                f(depth, entity, parent);
            }
        }
    }

    /// Per-stratum parallel cascade helper processing strata `0..=D_max` sequentially with a
    /// strict synchronization barrier between depths, dispatching disjoint slices of each stratum
    /// across Bevy's [`ComputeTaskPool`].
    ///
    /// Every parent at depth `d` completes before any child at depth `d + 1` begins. Runs
    /// sequentially under Miri (`cfg!(miri)`) or when a stratum fits in a single grain.
    pub fn par_for_each_stratum<F>(&self, relation: ComponentId, f: F)
    where
        F: Fn(usize, &[Entity]) + Sync,
    {
        let Some(entry) = self.by_relation.get(&relation) else {
            return;
        };
        let pool_opt: Option<&ComputeTaskPool> = if cfg!(miri) {
            None
        } else {
            Some(ComputeTaskPool::get_or_init(TaskPool::new))
        };
        let worker_count = pool_opt.map_or(1, |p| p.thread_num().max(1));

        for (depth, stratum) in entry.strata.iter().enumerate() {
            if stratum.is_empty() {
                continue;
            }
            let grain = (stratum.len() / (worker_count * 4).max(1)).max(256);
            if let Some(pool) = pool_opt
                && stratum.len() > grain
            {
                let f_ref = &f;
                pool.scope(|s| {
                    for chunk in stratum.chunks(grain) {
                        s.spawn(async move {
                            f_ref(depth, chunk);
                        });
                    }
                });
            } else {
                f(depth, stratum.as_slice());
            }
        }
    }

    /// Convenience per-stratum parallel cascade helper invoking `f(depth, entity, parent)` for
    /// each entity in `stratum[0..=D_max]` with a strict parent-before-child depth barrier.
    pub fn par_for_each_entity_in_strata<F>(&self, relation: ComponentId, f: F)
    where
        F: Fn(usize, Entity, Option<Entity>) + Sync,
    {
        self.par_for_each_stratum(relation, |depth, chunk| {
            for &entity in chunk {
                let parent = self.parent_of(relation, entity);
                f(depth, entity, parent);
            }
        });
    }

    /// Cycle-guarded upward ancestor walk over the cached parent map starting from `entity`'s
    /// direct parent, returning the nearest ancestor satisfying `pred` (never `entity` itself).
    pub fn find_nearest_ancestor(
        &self,
        relation: ComponentId,
        entity: Entity,
        mut pred: impl FnMut(Entity) -> bool,
    ) -> Option<Entity> {
        let entry = self.by_relation.get(&relation)?;
        let mut power = 1usize;
        let mut lam = 0usize;
        let mut tortoise = entity;
        let mut curr = entry.parent_map.get(&entity).copied()?;
        loop {
            if curr == tortoise || curr == entity {
                return None;
            }
            if pred(curr) {
                return Some(curr);
            }
            lam += 1;
            if lam == power {
                tortoise = curr;
                power = power.saturating_mul(2);
                lam = 0;
            }
            curr = entry.parent_map.get(&curr).copied()?;
        }
    }
}

// ============================================================================
// ABI 0.3.0 `host_iter_up_cascade` Implementation
// ============================================================================

#[inline]
fn resolve_dynamic_target_parent(
    world: UnsafeWorldCell<'_>,
    target_cid: ComponentId,
    child: Entity,
) -> Option<Entity> {
    let child_ffi = FfiEntity::from_bits(child.to_bits());
    // SAFETY: `DynamicComponentHookMap` is only mutated during exclusive `LoadContext` or
    // `DeferredWorld` hooks, never concurrently with system execution.
    let hook_map = unsafe { world.get_resource::<DynamicComponentHookMap>() }?;
    let info = world.components().get_info(target_cid)?;
    // SAFETY: Read-only storage metadata inspection during active system execution.
    let storages = unsafe { world.storages() };
    match info.storage_type() {
        StorageType::Table => {
            for table in storages.tables.iter() {
                if !table.has_column(target_cid) {
                    continue;
                }
                for &parent in table.entities() {
                    if hook_map
                        .relationship_targets_for(target_cid, parent)
                        .contains(&child_ffi)
                    {
                        return (parent != Entity::PLACEHOLDER
                            && world.entities().contains_spawned(parent))
                        .then_some(parent);
                    }
                }
            }
        }
        StorageType::SparseSet => {
            for arch in world.archetypes().iter() {
                if !arch.contains(target_cid) {
                    continue;
                }
                for arch_ent in arch.entities() {
                    let parent = arch_ent.id();
                    if hook_map
                        .relationship_targets_for(target_cid, parent)
                        .contains(&child_ffi)
                    {
                        return (parent != Entity::PLACEHOLDER
                            && world.entities().contains_spawned(parent))
                        .then_some(parent);
                    }
                }
            }
        }
    }
    None
}

#[inline]
fn read_parent_from_world(
    world: UnsafeWorldCell<'_>,
    rel_cid: ComponentId,
    layout: RelationLayout,
    entity: Entity,
) -> Option<Entity> {
    if layout == RelationLayout::DynamicTarget {
        return resolve_dynamic_target_parent(world, rel_cid, entity);
    }
    let ent_cell = world.get_entity(entity).ok()?;
    // SAFETY: Shared read of the relation edge component during `host_iter_up_cascade`;
    // `extract_parent_from_raw` validates byte size according to `layout`.
    let ptr = unsafe { ent_cell.get_by_id(rel_cid) }?;
    // SAFETY: `ptr` points to initialized `rel_cid` bytes matching `layout`.
    unsafe { extract_parent_from_raw(layout, ptr.as_ptr(), world.entities()) }
}

/// Core implementation backing ABI `0.3.0` [`crate::host_iter_up_cascade`].
///
/// Walks upward from `entity`'s parent along `relation` (`BamComponent::INVALID` defaults to
/// Bevy's built-in [`ChildOf`]) with Brent's zero-allocation cycle guard until locating the nearest
/// ancestor owning the component declared at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_ptr` must point to aligned writable `*const c_void` storage, or null.
/// - `out_owner` must point to aligned writable [`FfiEntity`] storage, or null.
pub(crate) unsafe fn iter_up_cascade_impl(
    iter: *mut BamIter,
    relation: BamComponent,
    entity: FfiEntity,
    term_index: usize,
    out_ptr: *mut *const c_void,
    out_owner: *mut FfiEntity,
) -> FfiResult {
    if out_ptr.is_null() || out_owner.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_ptr.is_aligned() || !out_owner.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_ptr` and `out_owner` are non-null, aligned, and writable.
    unsafe {
        *out_ptr = core::ptr::null();
        *out_owner = FfiEntity::NULL;
    }

    // SAFETY: Validates `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };

    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INVALID_ENTITY;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INVALID_ENTITY;
    }

    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.has_entity_column_data()
        || term.is_resource()
        || term.is_message()
        || (host_iter.plan.or_term_mask & (1u32 << term_index)) != 0
    {
        return FfiResult::PERMISSION_DENIED;
    }

    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    if world.get_entity(bevy_entity).is_err() {
        return FfiResult::INVALID_ENTITY;
    }

    // Resolve `relation` (`BamComponent::INVALID` -> built-in `ChildOf`; otherwise query plan or
    // `ModRegistry` lookup for explicit `ChildOf` or custom entity-valued parent components).
    let (rel_cid, rel_layout) = if !relation.is_valid() {
        let Some(child_of_cid) = world.components().component_id::<ChildOf>() else {
            return FfiResult::NOT_FOUND;
        };
        (child_of_cid, RelationLayout::ChildOf)
    } else {
        let resolved_cid = if let Some(plan_term) = host_iter
            .plan
            .terms
            .iter()
            .find(|t| t.component == relation)
        {
            if plan_term.is_resource() || plan_term.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            plan_term.component_id
        } else {
            // SAFETY: `ModRegistry` is never mutably borrowed concurrently with system execution.
            let Some(reg) = (unsafe { world.get_resource::<ModRegistry>() }) else {
                return FfiResult::INVALID_COMPONENT;
            };
            let Some(entry) = reg.snapshot().get_by_handle(relation) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if entry.is_resource() || entry.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            entry.component_id
        };

        let Some(info) = world.components().get_info(resolved_cid) else {
            return FfiResult::INVALID_COMPONENT;
        };
        let Some(layout) = classify_relation_layout(world, resolved_cid, info.layout().size())
        else {
            return FfiResult::INVALID_COMPONENT;
        };
        (resolved_cid, layout)
    };

    // Brent's zero-allocation cycle-guarded upward walk starting from `entity`'s direct parent.
    let mut power = 1usize;
    let mut lam = 0usize;
    let mut tortoise = bevy_entity;
    let Some(mut curr) = read_parent_from_world(world, rel_cid, rel_layout, bevy_entity) else {
        return FfiResult::NOT_FOUND;
    };

    loop {
        if curr == tortoise || curr == bevy_entity {
            return FfiResult::NOT_FOUND;
        }
        let Ok(ancestor_cell) = world.get_entity(curr) else {
            return FfiResult::NOT_FOUND;
        };
        // SAFETY: `term_index` declared entity column read or write access on `term.component_id`
        // in `host_iter.plan` / Bevy's `FilteredAccessSet`.
        if let Some(comp_ptr) = unsafe { ancestor_cell.get_by_id(term.component_id) } {
            // SAFETY: `out_ptr` and `out_owner` are non-null, aligned, and writable.
            unsafe {
                *out_ptr = comp_ptr.as_ptr().cast::<c_void>();
                *out_owner = FfiEntity::from_bits(curr.to_bits());
            }
            return FfiResult::OK;
        }
        lam += 1;
        if lam == power {
            tortoise = curr;
            power = power.saturating_mul(2);
            lam = 0;
        }
        let Some(next) = read_parent_from_world(world, rel_cid, rel_layout, curr) else {
            return FfiResult::NOT_FOUND;
        };
        curr = next;
    }
}

#[cfg(test)]
mod tests {
    use core::ffi::{CStr, c_void};
    use core::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    use bevy_abi_mod_sys::{
        AccessMode, BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BamCommands, BamComponent, BamQueryDesc,
        BamSystemCtx, BamSystemDesc, BamTermDesc, ComponentDesc, ComponentUuid, FfiChildOf,
        FfiEntity, FfiResult, ModComponent,
    };
    use bevy_ecs::{
        hierarchy::{ChildOf, Children},
        prelude::{Component, Entity},
        ptr::OwningPtr,
        schedule::Schedule,
        world::World,
    };

    use super::{ensure_children_registered_in_world, host_sys_entity_children};
    use crate::{
        ffi::commands::{host_cmd_insert, host_cmd_remove, host_cmd_spawn},
        ffi::register::{
            DynamicComponentHookMap, LoadContext, ensure_dynamic_hook_map_in_world,
            host_register_component, host_register_system,
        },
        loader::ModId,
        registry::ModRegistry,
        system::host_sys_commands,
    };

    #[repr(C)]
    #[derive(Component, Copy, Clone, Default)]
    struct TestPos(u32);

    impl ModComponent for TestPos {
        const CANONICAL_NAME: &'static str = "hier_test::TestPos";
        const COMPONENT_CSTR: &'static CStr = c"hier_test::TestPos";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("hier_test::TestPos");
        const IS_RAW_INSERTABLE: bool = true;
        const IS_ABI_POD: bool = true;
        type VTable = ();

        fn vtable() -> &'static Self::VTable {
            &()
        }

        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
            &[]
        }
    }

    #[derive(Component)]
    #[relationship(relationship_target = CustomFollowers)]
    struct CustomFollowing(#[allow(dead_code)] Entity);

    #[derive(Component)]
    #[relationship_target(relationship = CustomFollowing)]
    struct CustomFollowers(Vec<Entity>);

    impl core::ops::Deref for CustomFollowers {
        type Target = [Entity];

        fn deref(&self) -> &[Entity] {
            &self.0
        }
    }

    struct ChildrenReadProbe {
        target: FfiEntity,
        rel_target: BamComponent,
        out_status: Mutex<FfiResult>,
        out_ptr_addr: Mutex<usize>,
        out_entities: Mutex<Vec<FfiEntity>>,
    }

    unsafe extern "C" fn probe_children_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `ChildrenReadProbe`.
        let probe = unsafe { &*user_data.cast::<ChildrenReadProbe>() };
        let mut ptr: *const FfiEntity = core::ptr::null();
        let mut len: usize = 0;
        // SAFETY: `sys_ctx` is active during `GuestSystem` run and `ptr`/`len` are writable.
        let status = unsafe {
            host_sys_entity_children(
                sys_ctx.cast_const(),
                probe.target,
                probe.rel_target,
                &mut ptr,
                &mut len,
            )
        };
        *probe.out_status.lock().unwrap() = status;
        *probe.out_ptr_addr.lock().unwrap() = ptr as usize;
        if status.is_ok() && len > 0 && !ptr.is_null() {
            // SAFETY: `host_sys_entity_children` validated `ptr[0..len]` for this system tick.
            let slice = unsafe { core::slice::from_raw_parts(ptr, len) };
            *probe.out_entities.lock().unwrap() = slice.to_vec();
        } else {
            probe.out_entities.lock().unwrap().clear();
        }
        FfiResult::OK
    }

    struct HierarchyCmdPlan {
        child_of_handle: BamComponent,
        pos_handle: BamComponent,
        parent: FfiEntity,
        existing_child: FfiEntity,
        detach_child: FfiEntity,
        spawned_child: Mutex<FfiEntity>,
    }

    unsafe extern "C" fn apply_hierarchy_cmds_system(
        sys_ctx: *mut BamSystemCtx,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `HierarchyCmdPlan`.
        let plan = unsafe { &*user_data.cast::<HierarchyCmdPlan>() };
        let mut cmds: *mut BamCommands = core::ptr::null_mut();
        // SAFETY: `sys_ctx` is an active `HostSystemCtx`.
        let rc = unsafe { host_sys_commands(sys_ctx, &mut cmds) };
        assert!(rc.is_ok(), "host_sys_commands failed: {rc}");

        // 1. add_child via `host_cmd_insert(FfiChildOf)`
        let link = FfiChildOf::new(plan.parent);
        // SAFETY: `cmds` is active and `link` matches `FfiChildOf` layout.
        let r_add = unsafe {
            host_cmd_insert(
                cmds,
                plan.existing_child,
                plan.child_of_handle,
                (&raw const link).cast::<u8>(),
                core::mem::size_of::<FfiChildOf>(),
            )
        };
        assert!(r_add.is_ok(), "host_cmd_insert failed: {r_add}");

        // 2. remove_parent via `host_cmd_remove(ChildOf)`
        // SAFETY: `cmds` is active.
        let r_rem = unsafe { host_cmd_remove(cmds, plan.detach_child, plan.child_of_handle) };
        assert!(r_rem.is_ok(), "host_cmd_remove failed: {r_rem}");

        // 3. spawn_child via `host_cmd_spawn([ChildOf, TestPos])`
        let pos = TestPos(77);
        let comps = [plan.child_of_handle, plan.pos_handle];
        let ptrs = [
            (&raw const link).cast::<u8>(),
            (&raw const pos).cast::<u8>(),
        ];
        let mut spawned = FfiEntity::NULL;
        // SAFETY: `cmds` is active and `comps`/`ptrs` match registered component layouts.
        let r_spawn = unsafe {
            host_cmd_spawn(
                cmds,
                comps.as_ptr(),
                ptrs.as_ptr(),
                comps.len(),
                &mut spawned,
            )
        };
        assert!(r_spawn.is_ok(), "host_cmd_spawn failed: {r_spawn}");
        *plan.spawned_child.lock().unwrap() = spawned;
        FfiResult::OK
    }

    #[test]
    fn children_zero_copy_slice_and_undeclared_permission_denied() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let pos_cid = world.register_component::<TestPos>();
        let pos_handle = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        world.insert_resource(reg.clone());

        // Idempotent world registration check.
        ensure_children_registered_in_world(&mut world);
        ensure_children_registered_in_world(&mut world);
        reg = world.resource::<ModRegistry>().clone();

        let parent = world.spawn(TestPos(1)).id();
        let child1 = world.spawn((TestPos(2), ChildOf(parent))).id();
        let child2 = world.spawn((TestPos(3), ChildOf(parent))).id();
        let leaf = world.spawn(TestPos(4)).id();

        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let child1_ffi = FfiEntity::from_bits(child1.to_bits());
        let child2_ffi = FfiEntity::from_bits(child2.to_bits());
        let leaf_ffi = FfiEntity::from_bits(leaf.to_bits());

        let children_handle = reg.get_by_name(BAM_CHILDREN_NAME).unwrap().handle;

        let active = Arc::new(AtomicBool::new(true));
        let probe_ok = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_no_access = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_filter_only = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });

        let systems = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(1)),
            );

            // System 1: declares READ on Children.
            let t_read = [BamTermDesc::new(children_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s1 = BamSystemDesc::new(
                c"probe_ok".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe_ok as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs1 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s1) };
            assert_eq!(rs1, FfiResult::OK);

            // System 2: declares READ on TestPos only (NO Children read access).
            let t_pos = [BamTermDesc::new(pos_handle, AccessMode::READ)];
            let q_pos = [BamQueryDesc::new(t_pos.as_ptr(), t_pos.len())];
            let s2 = BamSystemDesc::new(
                c"probe_denied".as_ptr(),
                q_pos.as_ptr(),
                q_pos.len(),
                probe_children_system,
                (&*probe_no_access as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs2 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s2) };
            assert_eq!(rs2, FfiResult::OK);

            // System 3: declares With<Children> (FILTER_ONLY, NOT component data read).
            let t_filter = [BamTermDesc::new(children_handle, AccessMode::FILTER_ONLY)];
            let q_filter = [BamQueryDesc::new(t_filter.as_ptr(), t_filter.len())];
            let s3 = BamSystemDesc::new(
                c"probe_filter_only".as_ptr(),
                q_filter.as_ptr(),
                q_filter.len(),
                probe_children_system,
                (&*probe_filter_only as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs3 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s3) };
            assert_eq!(rs3, FfiResult::OK);

            ctx.finish_init();
            core::mem::take(&mut ctx.registered_systems)
        };

        let mut sched = Schedule::default();
        for sys in systems {
            sched.add_systems(sys);
        }
        sched.run(&mut world);

        assert_eq!(*probe_ok.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(
            *probe_ok.out_entities.lock().unwrap(),
            vec![child1_ffi, child2_ffi]
        );
        // Zero-copy guarantee: returned pointer matches Bevy's native `Children` backing buffer.
        let expected_addr = world.entity(parent).get::<Children>().unwrap().as_ptr() as usize;
        assert_eq!(*probe_ok.out_ptr_addr.lock().unwrap(), expected_addr);

        // Undeclared / filter-only access MUST return PERMISSION_DENIED.
        assert_eq!(
            *probe_no_access.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            *probe_filter_only.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );

        // Leaf without Children returns Ok(&[]) and null/dead entity returns INVALID_ENTITY.
        let leaf_check = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                world.resource::<ModRegistry>().clone(),
                active,
                &mut world as *mut World,
                Some(ModId(2)),
            );
            let probe_leaf = Box::new(ChildrenReadProbe {
                target: leaf_ffi,
                rel_target: BamComponent::INVALID,
                out_status: Mutex::new(FfiResult::UNSUPPORTED),
                out_ptr_addr: Mutex::new(1),
                out_entities: Mutex::new(Vec::new()),
            });
            let t_read = [BamTermDesc::new(children_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s_leaf = BamSystemDesc::new(
                c"probe_leaf".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe_leaf as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rl = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_leaf) };
            assert_eq!(rl, FfiResult::OK);
            ctx.finish_init();
            (core::mem::take(&mut ctx.registered_systems), probe_leaf)
        };
        let mut leaf_sched = Schedule::default();
        for sys in leaf_check.0 {
            leaf_sched.add_systems(sys);
        }
        leaf_sched.run(&mut world);
        assert_eq!(*leaf_check.1.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(*leaf_check.1.out_ptr_addr.lock().unwrap(), 0);
        assert!(leaf_check.1.out_entities.lock().unwrap().is_empty());

        // Null / uninitialized ctx & pointers checks.
        let mut p: *const FfiEntity = core::ptr::null();
        let mut l: usize = 0;
        // SAFETY: Null ctx handled safely.
        let r_null = unsafe {
            host_sys_entity_children(
                core::ptr::null(),
                parent_ffi,
                BamComponent::INVALID,
                &mut p,
                &mut l,
            )
        };
        assert_eq!(r_null, FfiResult::NULL_POINTER);
    }

    #[test]
    fn ffi_child_of_commands_add_spawn_and_remove_parent_update_children() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let pos_cid = world.register_component::<TestPos>();
        let pos_handle = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        let child_of_cid = world.register_component::<ChildOf>();
        let child_of_handle = reg.register_hierarchy(child_of_cid, true).unwrap();
        world.insert_resource(reg.clone());

        let parent = world.spawn_empty().id();
        let existing_child = world.spawn_empty().id();
        let detach_child = world.spawn(ChildOf(parent)).id();

        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let existing_ffi = FfiEntity::from_bits(existing_child.to_bits());
        let detach_ffi = FfiEntity::from_bits(detach_child.to_bits());

        let active = Arc::new(AtomicBool::new(true));
        let plan = Box::new(HierarchyCmdPlan {
            child_of_handle,
            pos_handle,
            parent: parent_ffi,
            existing_child: existing_ffi,
            detach_child: detach_ffi,
            spawned_child: Mutex::new(FfiEntity::NULL),
        });
        let probe = Box::new(ChildrenReadProbe {
            target: parent_ffi,
            rel_target: BamComponent::INVALID,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });

        let (sys_cmds, sys_probe, children_handle) = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                active,
                &mut world as *mut World,
                Some(ModId(3)),
            );
            let ch_handle = ctx
                .registry
                .get_by_name(BAM_CHILDREN_NAME)
                .expect("LoadContext auto-registers Children")
                .handle;
            let s_cmd = BamSystemDesc::new(
                c"apply_hierarchy_cmds".as_ptr(),
                core::ptr::null(),
                0,
                apply_hierarchy_cmds_system,
                (&*plan as *const HierarchyCmdPlan)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rc1 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_cmd) };
            assert_eq!(rc1, FfiResult::OK);
            let t_read = [BamTermDesc::new(ch_handle, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s_probe = BamSystemDesc::new(
                c"probe_after_cmds".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rc2 = unsafe { host_register_system(ctx.as_ffi_ptr(), &s_probe) };
            assert_eq!(rc2, FfiResult::OK);
            ctx.finish_init();
            let mut systems = core::mem::take(&mut ctx.registered_systems);
            let s2 = systems.pop().unwrap();
            let s1 = systems.pop().unwrap();
            (s1, s2, ch_handle)
        };

        assert!(children_handle.is_valid());
        let mut cmd_sched = Schedule::default();
        cmd_sched.add_systems(sys_cmds);
        cmd_sched.run(&mut world);

        let spawned_ffi = *plan.spawned_child.lock().unwrap();
        assert!(!spawned_ffi.is_null());
        let spawned_ent = Entity::try_from_bits(spawned_ffi.to_bits()).unwrap();
        assert_eq!(world.entity(spawned_ent).get::<TestPos>().unwrap().0, 77);
        assert!(world.entity(detach_child).get::<ChildOf>().is_none());

        let mut probe_sched = Schedule::default();
        probe_sched.add_systems(sys_probe);
        probe_sched.run(&mut world);

        assert_eq!(*probe.out_status.lock().unwrap(), FfiResult::OK);
        let read_children = probe.out_entities.lock().unwrap().clone();
        assert_eq!(read_children.len(), 2);
        assert!(read_children.contains(&existing_ffi));
        assert!(read_children.contains(&spawned_ffi));
        assert!(!read_children.contains(&detach_ffi));
        assert_eq!(BAM_CHILD_OF_NAME, "bevy_ecs::hierarchy::ChildOf");
    }

    #[test]
    fn dynamic_relationship_pair_attach_retarget_self_guard_despawn_and_zero_copy_read() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));

        let mut rel_h = BamComponent::INVALID;
        let mut tgt_h = BamComponent::INVALID;
        let mut bind_out = BamComponent::INVALID;

        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(4)),
            );
            let rel_desc = ComponentDesc::new(
                c"space::Orbiting".as_ptr(),
                ComponentUuid::from_parts("space::Orbiting", 8, 8, 0),
                8,
                8,
            );
            let tgt_desc = ComponentDesc::new(
                c"space::Orbiters".as_ptr(),
                ComponentUuid::from_parts("space::Orbiters", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &rel_desc, &mut rel_h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &tgt_desc, &mut tgt_h),
                    FfiResult::OK
                );
            }
            let bind_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, tgt_h.0))
                    .unwrap();
            let bind_desc = ComponentDesc::new(
                bind_name.as_ptr(),
                ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and synthetic `__bam_rel_bind` descriptor.
            let rb =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &mut bind_out) };
            assert_eq!(rb, FfiResult::OK);
            assert_eq!(bind_out, tgt_h);

            // Idempotent re-bind succeeds; wrong self-bind fails.
            let mut rebind = BamComponent::INVALID;
            // SAFETY: Valid `LoadContext` and synthetic `__bam_rel_bind` descriptor.
            let rr = unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &mut rebind) };
            assert_eq!(rr, FfiResult::OK);
            let bad_self_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, rel_h.0))
                    .unwrap();
            let bad_self_desc = ComponentDesc::new(
                bad_self_name.as_ptr(),
                ComponentUuid::from_parts("bad_self", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and invalid self-bind descriptor.
            let r_bad =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &bad_self_desc, &mut rebind) };
            assert_eq!(r_bad, FfiResult::TYPE_MISMATCH);

            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let rel_cid = reg_after.get_by_handle(rel_h).unwrap().component_id;
        let tgt_cid = reg_after.get_by_handle(tgt_h).unwrap().component_id;

        let sun1 = world.spawn_empty().id();
        let sun2 = world.spawn_empty().id();
        let planet1 = world.spawn_empty().id();
        let planet2 = world.spawn_empty().id();
        let sun1_ffi = FfiEntity::from_bits(sun1.to_bits());
        let sun2_ffi = FfiEntity::from_bits(sun2.to_bits());
        let planet1_ffi = FfiEntity::from_bits(planet1.to_bits());
        let planet2_ffi = FfiEntity::from_bits(planet2.to_bits());

        // 1. on_insert attach: planet1 & planet2 orbit sun1.
        let mut target_bits_1 = sun1_ffi.to_bits();
        let mut target_bits_2 = sun1_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout holding `FfiEntity` (`u64` bits).
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut target_bits_1).cast()),
            );
            world.entity_mut(planet2).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut target_bits_2).cast()),
            );
        }
        assert!(world.entity(sun1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1),
            &[planet1_ffi, planet2_ffi]
        );

        // Zero-copy read via scheduled GuestSystem + undeclared PERMISSION_DENIED check.
        let probe_ok = Box::new(ChildrenReadProbe {
            target: sun1_ffi,
            rel_target: tgt_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let probe_denied = Box::new(ChildrenReadProbe {
            target: sun1_ffi,
            rel_target: tgt_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let systems = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg_after,
                active,
                &mut world as *mut World,
                Some(ModId(5)),
            );
            let t_ok = [BamTermDesc::new(tgt_h, AccessMode::READ)];
            let q_ok = [BamQueryDesc::new(t_ok.as_ptr(), t_ok.len())];
            let s_ok = BamSystemDesc::new(
                c"read_orbiters".as_ptr(),
                q_ok.as_ptr(),
                q_ok.len(),
                probe_children_system,
                (&*probe_ok as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            let s_den = BamSystemDesc::new(
                c"read_orbiters_denied".as_ptr(),
                core::ptr::null(),
                0,
                probe_children_system,
                (&*probe_denied as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`s.
            unsafe {
                assert_eq!(host_register_system(ctx.as_ffi_ptr(), &s_ok), FfiResult::OK);
                assert_eq!(
                    host_register_system(ctx.as_ffi_ptr(), &s_den),
                    FfiResult::OK
                );
            }
            ctx.finish_init();
            core::mem::take(&mut ctx.registered_systems)
        };
        let mut sched = Schedule::default();
        for sys in systems {
            sched.add_systems(sys);
        }
        sched.run(&mut world);

        assert_eq!(*probe_ok.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(
            *probe_ok.out_entities.lock().unwrap(),
            vec![planet1_ffi, planet2_ffi]
        );
        let map_slice_addr = world
            .resource::<DynamicComponentHookMap>()
            .relationship_targets_for(tgt_cid, sun1)
            .as_ptr() as usize;
        assert_eq!(*probe_ok.out_ptr_addr.lock().unwrap(), map_slice_addr);
        assert_eq!(
            *probe_denied.out_status.lock().unwrap(),
            FfiResult::PERMISSION_DENIED
        );

        // 2. Retarget overwrite (on_discard + on_insert): move planet1 then planet2 to sun2.
        let mut retarget_1 = sun2_ffi.to_bits();
        let mut retarget_2 = sun2_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut retarget_1).cast()),
            );
        }
        assert!(world.entity(sun1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1),
            &[planet2_ffi]
        );
        assert!(world.entity(sun2).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi]
        );

        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet2).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut retarget_2).cast()),
            );
        }
        assert!(!world.entity(sun1).contains_id(tgt_cid));
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun1)
                .is_empty()
        );
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi, planet2_ffi]
        );

        // 3. Self-referential guard: inserting Orbiting(planet1) on planet1 removes Orbiting.
        let mut self_bits = planet1_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut self_bits).cast()),
            );
        }
        assert!(!world.entity(planet1).contains_id(rel_cid));
        assert!(!world.entity(planet1).contains_id(tgt_cid));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet2_ffi]
        );

        // 4. Re-attach planet1 -> sun2, despawn planet2 (source despawn cleanup).
        let mut reattach_1 = sun2_ffi.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout.
        unsafe {
            world.entity_mut(planet1).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut reattach_1).cast()),
            );
        }
        assert!(world.despawn(planet2));
        assert_eq!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2),
            &[planet1_ffi]
        );

        // 5. Target despawn cleanup: despawn sun2 -> planet1 loses Orbiting cleanly.
        assert!(world.despawn(sun2));
        assert!(!world.entity(planet1).contains_id(rel_cid));
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .relationship_targets_for(tgt_cid, sun2)
                .is_empty()
        );
    }

    #[test]
    fn bridged_custom_relationship_target_and_load_ctx_rollback() {
        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut reg = ModRegistry::new();
        let followers_cid = world.register_component::<CustomFollowers>();
        let followers_h = reg
            .register_relationship_target::<CustomFollowers>(
                "custom_test::CustomFollowers",
                Some("CustomFollowers"),
                followers_cid,
            )
            .unwrap();

        let target = world.spawn_empty().id();
        let f1 = world.spawn(CustomFollowing(target)).id();
        let f2 = world.spawn(CustomFollowing(target)).id();
        let target_ffi = FfiEntity::from_bits(target.to_bits());
        let f1_ffi = FfiEntity::from_bits(f1.to_bits());
        let f2_ffi = FfiEntity::from_bits(f2.to_bits());

        let probe = Box::new(ChildrenReadProbe {
            target: target_ffi,
            rel_target: followers_h,
            out_status: Mutex::new(FfiResult::UNSUPPORTED),
            out_ptr_addr: Mutex::new(0),
            out_entities: Mutex::new(Vec::new()),
        });
        let active = Arc::new(AtomicBool::new(true));
        let sys = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                active,
                &mut world as *mut World,
                Some(ModId(6)),
            );
            let t_read = [BamTermDesc::new(followers_h, AccessMode::READ)];
            let q_read = [BamQueryDesc::new(t_read.as_ptr(), t_read.len())];
            let s = BamSystemDesc::new(
                c"probe_custom_rt".as_ptr(),
                q_read.as_ptr(),
                q_read.len(),
                probe_children_system,
                (&*probe as *const ChildrenReadProbe)
                    .cast_mut()
                    .cast::<c_void>(),
            );
            // SAFETY: Valid `LoadContext` and `BamSystemDesc`.
            let rs = unsafe { host_register_system(ctx.as_ffi_ptr(), &s) };
            assert_eq!(rs, FfiResult::OK);
            ctx.finish_init();
            ctx.registered_systems.pop().unwrap()
        };
        let mut sched = Schedule::default();
        sched.add_systems(sys);
        sched.run(&mut world);

        assert_eq!(*probe.out_status.lock().unwrap(), FfiResult::OK);
        assert_eq!(*probe.out_entities.lock().unwrap(), vec![f1_ffi, f2_ffi]);
        let expected_addr = world
            .entity(target)
            .get::<CustomFollowers>()
            .unwrap()
            .as_ptr() as usize;
        assert_eq!(*probe.out_ptr_addr.lock().unwrap(), expected_addr);

        // Verify `LoadContext::rollback()` cleans up uncommitted `__bam_rel_bind` pairs.
        let mut ctx_rb = LoadContext::with_world_and_mod_id(
            reg,
            Arc::new(AtomicBool::new(true)),
            &mut world as *mut World,
            Some(ModId(7)),
        );
        let mut h_r = BamComponent::INVALID;
        let mut h_t = BamComponent::INVALID;
        let mut h_b = BamComponent::INVALID;
        let d_r = ComponentDesc::new(
            c"rb::Rel".as_ptr(),
            ComponentUuid::from_parts("rb::Rel", 8, 8, 0),
            8,
            8,
        );
        let d_t = ComponentDesc::new(
            c"rb::Tgt".as_ptr(),
            ComponentUuid::from_parts("rb::Tgt", 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
        unsafe {
            assert_eq!(
                host_register_component(ctx_rb.as_ffi_ptr(), &d_r, &mut h_r),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx_rb.as_ffi_ptr(), &d_t, &mut h_t),
                FfiResult::OK
            );
        }
        let tgt_cid_rb = ctx_rb.registry.get_by_handle(h_t).unwrap().component_id;
        let bind_name =
            std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", h_r.0, h_t.0)).unwrap();
        let d_b = ComponentDesc::new(
            bind_name.as_ptr(),
            ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
            0,
            1,
        );
        // SAFETY: Valid `LoadContext` and `__bam_rel_bind` descriptor.
        let r_bind = unsafe { host_register_component(ctx_rb.as_ffi_ptr(), &d_b, &mut h_b) };
        assert_eq!(r_bind, FfiResult::OK);
        assert!(
            world
                .resource::<DynamicComponentHookMap>()
                .is_relationship_target(tgt_cid_rb)
        );
        ctx_rb.rollback();
        assert!(
            !world
                .resource::<DynamicComponentHookMap>()
                .is_relationship_target(tgt_cid_rb)
        );
    }

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    struct CustomParent8(FfiEntity);

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    struct CustomParent4(u32);

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    #[component(storage = "SparseSet")]
    struct SparseParent8(FfiEntity);

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    #[component(storage = "SparseSet")]
    struct SparseVal(u64);

    #[test]
    fn cascade_cache_strata_reuse_and_invalidation_on_change_and_removal() {
        use super::CascadeHierarchyCache;
        use bevy_platform::collections::HashSet;

        let mut world = World::new();
        let root = world.spawn(TestPos(10)).id();
        let a = world.spawn((TestPos(20), ChildOf(root))).id();
        let b = world.spawn((TestPos(30), ChildOf(root))).id();
        let c = world.spawn((TestPos(40), ChildOf(a))).id();
        let d = world.spawn((TestPos(50), ChildOf(c))).id();

        let child_of_id = world.components().component_id::<ChildOf>().unwrap();
        assert!(CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));

        {
            let cache = world.resource::<CascadeHierarchyCache>();
            assert_eq!(cache.rebuild_count(child_of_id), 1);
            assert!(!cache.has_cycles(child_of_id));
            assert_eq!(cache.strata(child_of_id).len(), 4);
            assert_eq!(cache.strata(child_of_id)[0], vec![root]);
            assert_eq!(cache.depth_of(child_of_id, root), Some(0));
            assert_eq!(cache.depth_of(child_of_id, a), Some(1));
            assert_eq!(cache.depth_of(child_of_id, b), Some(1));
            assert_eq!(cache.depth_of(child_of_id, c), Some(2));
            assert_eq!(cache.depth_of(child_of_id, d), Some(3));
            assert_eq!(cache.parent_of(child_of_id, root), None);
            assert_eq!(cache.parent_of(child_of_id, c), Some(a));
            assert_eq!(cache.parent_of(child_of_id, d), Some(c));

            // Single-pass cascade strictly visits parents before children.
            let mut visited = HashSet::new();
            cache.for_each_cascade(child_of_id, |depth, ent, parent| {
                if let Some(p) = parent {
                    assert!(visited.contains(&p), "parent must precede child");
                    assert_eq!(cache.depth_of(child_of_id, p), Some(depth - 1));
                } else {
                    assert_eq!(depth, 0);
                }
                assert!(visited.insert(ent));
            });
            assert_eq!(visited.len(), 5);

            // Per-stratum parallel helper covers all 5 entities with depth barrier.
            let seen_par = Mutex::new(Vec::<(usize, Entity)>::new());
            cache.par_for_each_entity_in_strata(child_of_id, |depth, ent, _| {
                seen_par.lock().unwrap().push((depth, ent));
            });
            let par_log = seen_par.into_inner().unwrap();
            assert_eq!(par_log.len(), 5);
            for w in par_log.windows(2) {
                assert!(w[0].0 <= w[1].0);
            }

            // `find_nearest_ancestor` walks upward cleanly.
            assert_eq!(
                cache.find_nearest_ancestor(child_of_id, d, |e| e == root),
                Some(root)
            );
            assert_eq!(
                cache.find_nearest_ancestor(child_of_id, root, |_| true),
                None
            );
        }

        // Mutating an UNRELATED component (`TestPos`) and advancing world ticks does ZERO rebuilds.
        world.entity_mut(d).get_mut::<TestPos>().unwrap().0 = 999;
        world.increment_change_tick();
        assert!(!CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));
        assert!(!CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));
        assert_eq!(
            world
                .resource::<CascadeHierarchyCache>()
                .rebuild_count(child_of_id),
            1
        );

        // Mutating a `ChildOf` edge (reparenting `c` directly under `root`) invalidates & rebuilds.
        world.entity_mut(c).insert(ChildOf(root));
        assert!(CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));
        {
            let cache = world.resource::<CascadeHierarchyCache>();
            assert_eq!(cache.rebuild_count(child_of_id), 2);
            assert_eq!(cache.strata(child_of_id).len(), 3);
            assert_eq!(cache.parent_of(child_of_id, c), Some(root));
            assert_eq!(cache.depth_of(child_of_id, c), Some(1));
            assert_eq!(cache.depth_of(child_of_id, d), Some(2));
        }

        // Removing `ChildOf` (`RemovedComponents`) invalidates & rebuilds, promoting `c` to root.
        world.entity_mut(c).remove::<ChildOf>();
        assert!(CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));
        {
            let cache = world.resource::<CascadeHierarchyCache>();
            assert_eq!(cache.rebuild_count(child_of_id), 3);
            assert_eq!(cache.parent_of(child_of_id, c), None);
            assert_eq!(cache.depth_of(child_of_id, c), Some(0));
            assert_eq!(cache.depth_of(child_of_id, d), Some(1));
            assert_eq!(cache.strata(child_of_id).len(), 2);
        }

        // Explicit invalidate & clear work as expected.
        world
            .resource_mut::<CascadeHierarchyCache>()
            .invalidate(child_of_id);
        assert!(CascadeHierarchyCache::ensure_built_in_world(
            &mut world,
            child_of_id
        ));
        assert_eq!(
            world
                .resource::<CascadeHierarchyCache>()
                .rebuild_count(child_of_id),
            4
        );
        world.resource_mut::<CascadeHierarchyCache>().clear();
        assert!(
            world
                .resource::<CascadeHierarchyCache>()
                .strata(child_of_id)
                .is_empty()
        );
    }

    #[test]
    fn cascade_cache_custom_8b_4b_sparse_and_dynamic_hook_relationships() {
        use super::CascadeHierarchyCache;

        let mut world = World::new();
        ensure_dynamic_hook_map_in_world(&mut world);
        let p8_cid = world.register_component::<CustomParent8>();
        let p4_cid = world.register_component::<CustomParent4>();
        let sp_cid = world.register_component::<SparseParent8>();

        let r = world.spawn_empty().id();
        let c1 = world
            .spawn((
                CustomParent8(FfiEntity::from_bits(r.to_bits())),
                CustomParent4(r.index_u32()),
                SparseParent8(FfiEntity::from_bits(r.to_bits())),
            ))
            .id();
        let c2 = world
            .spawn((
                CustomParent8(FfiEntity::from_bits(c1.to_bits())),
                CustomParent4(c1.index_u32()),
                SparseParent8(FfiEntity::from_bits(c1.to_bits())),
            ))
            .id();

        let mut cache = CascadeHierarchyCache::new();
        for cid in [p8_cid, p4_cid, sp_cid] {
            assert!(cache.ensure_built(&world, cid));
            assert!(!cache.ensure_built(&world, cid));
            assert_eq!(cache.rebuild_count(cid), 1);
            assert_eq!(cache.strata(cid).len(), 3);
            assert_eq!(cache.depth_of(cid, r), Some(0));
            assert_eq!(cache.depth_of(cid, c1), Some(1));
            assert_eq!(cache.depth_of(cid, c2), Some(2));
            assert_eq!(cache.parent_of(cid, c1), Some(r));
            assert_eq!(cache.parent_of(cid, c2), Some(c1));
        }

        // Mutate sparse parent -> invalidates `sp_cid` only, not `p8_cid` or `p4_cid`!
        world
            .entity_mut(c2)
            .insert(SparseParent8(FfiEntity::from_bits(r.to_bits())));
        assert!(!cache.ensure_built(&world, p8_cid));
        assert!(!cache.ensure_built(&world, p4_cid));
        assert!(cache.ensure_built(&world, sp_cid));
        assert_eq!(cache.depth_of(sp_cid, c2), Some(1));

        // Despawn root `r` -> `p4_cid` / `p8_cid` catch dead stratum-0 root & promote `c1` to depth 0.
        assert!(world.despawn(r));
        assert!(cache.ensure_built(&world, p4_cid));
        assert_eq!(cache.parent_of(p4_cid, c1), None);
        assert_eq!(cache.depth_of(p4_cid, c1), Some(0));
        assert_eq!(cache.depth_of(p4_cid, c2), Some(1));

        // DynamicComponentHookMap target (`Orbiters` zero-sized target) cache build + invalidation.
        let mut rel_h = BamComponent::INVALID;
        let mut tgt_h = BamComponent::INVALID;
        let mut bind_out = BamComponent::INVALID;
        let (rel_cid, tgt_cid) = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                ModRegistry::new(),
                Arc::new(AtomicBool::new(true)),
                &raw mut world,
                Some(ModId(9)),
            );
            let rel_desc = ComponentDesc::new(
                c"cascade::DynRel".as_ptr(),
                ComponentUuid::from_parts("cascade::DynRel", 8, 8, 0),
                8,
                8,
            );
            let tgt_desc = ComponentDesc::new(
                c"cascade::DynTgt".as_ptr(),
                ComponentUuid::from_parts("cascade::DynTgt", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &rel_desc, &mut rel_h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &tgt_desc, &mut tgt_h),
                    FfiResult::OK
                );
            }
            let bind_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, tgt_h.0))
                    .unwrap();
            let bind_desc = ComponentDesc::new(
                bind_name.as_ptr(),
                ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and `__bam_rel_bind` descriptor.
            let rb =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &bind_desc, &mut bind_out) };
            assert_eq!(rb, FfiResult::OK);
            let r_cid = ctx.registry.get_by_handle(rel_h).unwrap().component_id;
            let t_cid = ctx.registry.get_by_handle(tgt_h).unwrap().component_id;
            ctx.finish_init();
            (r_cid, t_cid)
        };

        let sun = world.spawn_empty().id();
        let planet = world.spawn_empty().id();
        let moon = world.spawn_empty().id();
        let mut sun_bits = sun.to_bits();
        let mut planet_bits = planet.to_bits();
        // SAFETY: `rel_cid` has `(8, 8)` layout storing `FfiEntity`.
        unsafe {
            world.entity_mut(planet).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut sun_bits).cast()),
            );
            world.entity_mut(moon).insert_by_id(
                rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut planet_bits).cast()),
            );
        }
        assert!(cache.ensure_built(&world, tgt_cid));
        assert!(!cache.ensure_built(&world, tgt_cid));
        assert_eq!(cache.strata(tgt_cid).len(), 3);
        assert_eq!(cache.parent_of(tgt_cid, planet), Some(sun));
        assert_eq!(cache.parent_of(tgt_cid, moon), Some(planet));
        assert_eq!(cache.depth_of(tgt_cid, sun), Some(0));
        assert_eq!(cache.depth_of(tgt_cid, moon), Some(2));
    }

    #[test]
    fn cascade_cache_and_up_cascade_cycle_guards() {
        use super::{CascadeHierarchyCache, iter_up_cascade_impl};
        use crate::ffi::iter::{HostIter, QueryPlan, QueryTermMeta};
        use bevy_ecs::change_detection::Tick;

        let mut world = World::new();
        let pos_cid = world.register_component::<TestPos>();
        let p8_cid = world.register_component::<CustomParent8>();

        // 1. Self-loop `s -> s`
        let s = world.spawn(TestPos(1)).id();
        world
            .entity_mut(s)
            .insert(CustomParent8(FfiEntity::from_bits(s.to_bits())));

        // 2. 2-cycle `a -> b -> a` (`b` has `TestPos`, `a` does not)
        let a = world.spawn_empty().id();
        let b = world.spawn(TestPos(22)).id();
        world
            .entity_mut(a)
            .insert(CustomParent8(FfiEntity::from_bits(b.to_bits())));
        world
            .entity_mut(b)
            .insert(CustomParent8(FfiEntity::from_bits(a.to_bits())));

        // 3. Lasso `x -> y -> c -> d -> c` (none has `TestPos`)
        let c = world.spawn_empty().id();
        let d = world.spawn_empty().id();
        let y = world.spawn_empty().id();
        let x = world.spawn_empty().id();
        world
            .entity_mut(c)
            .insert(CustomParent8(FfiEntity::from_bits(d.to_bits())));
        world
            .entity_mut(d)
            .insert(CustomParent8(FfiEntity::from_bits(c.to_bits())));
        world
            .entity_mut(y)
            .insert(CustomParent8(FfiEntity::from_bits(c.to_bits())));
        world
            .entity_mut(x)
            .insert(CustomParent8(FfiEntity::from_bits(y.to_bits())));

        let mut cache = CascadeHierarchyCache::new();
        assert!(cache.ensure_built(&world, p8_cid));
        assert!(cache.has_cycles(p8_cid));
        // Every node appears exactly once across all strata.
        let total_nodes: usize = cache.strata(p8_cid).iter().map(Vec::len).sum();
        assert_eq!(total_nodes, 7);

        let pos_h = BamComponent::from_raw(1);
        let p8_h = BamComponent::from_raw(2);
        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: pos_h,
                component_id: pos_cid,
                mode: AccessMode::READ,
                uuid: TestPos::UUID,
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: p8_h,
                component_id: p8_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("hier::CustomParent8"),
                size: 8,
                align: 8,
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

        let mut out_ptr: *const c_void = core::ptr::null();
        let mut out_owner = FfiEntity::NULL;

        // SAFETY: Valid `iter` and aligned stack outputs.
        unsafe {
            // Self-loop: must NOT return `s` as its own ancestor; returns `NOT_FOUND` and zeros outputs.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    p8_h,
                    FfiEntity::from_bits(s.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NOT_FOUND
            );
            assert!(out_ptr.is_null());
            assert_eq!(out_owner, FfiEntity::NULL);

            // 2-cycle from `a`: parent `b` has `TestPos(22)` -> found before cycle closes!
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    p8_h,
                    FfiEntity::from_bits(a.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(b.to_bits()));
            assert_eq!((*out_ptr.cast::<TestPos>()).0, 22);

            // 2-cycle from `b`: `a` lacks `TestPos`, next is `b` (self) -> cycle guard -> `NOT_FOUND`!
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    p8_h,
                    FfiEntity::from_bits(b.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NOT_FOUND
            );
            assert!(out_ptr.is_null());
            assert_eq!(out_owner, FfiEntity::NULL);

            // Lasso cycle `x -> y -> c -> d -> c` without `TestPos`: terminates with `NOT_FOUND`.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    p8_h,
                    FfiEntity::from_bits(x.to_bits()),
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

    #[test]
    fn iter_up_cascade_default_child_of_explicit_custom_sparse_and_permissions() {
        use super::iter_up_cascade_impl;
        use crate::ffi::iter::{HostIter, QueryPlan, QueryTermMeta};
        use bevy_abi_mod_sys::BAM_STORAGE_FLAG_SPARSE_SET;
        use bevy_ecs::change_detection::Tick;

        let mut world = World::new();
        let pos_cid = world.register_component::<TestPos>();
        let sparse_cid = world.register_component::<SparseVal>();
        let child_of_cid = world.register_component::<ChildOf>();
        let p4_cid = world.register_component::<CustomParent4>();

        let mut reg = ModRegistry::new();
        let pos_h = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        let child_of_h = reg.register_hierarchy(child_of_cid, true).unwrap();
        let sparse_h = BamComponent::from_raw(99);
        let p4_h = BamComponent::from_raw(100);
        world.insert_resource(reg);

        // Build chain: root(TestPos(100), SparseVal(777)) -> mid(no TestPos) -> leaf(TestPos(5))
        let root = world.spawn((TestPos(100), SparseVal(777))).id();
        let mid = world
            .spawn((ChildOf(root), CustomParent4(root.index_u32())))
            .id();
        let leaf = world
            .spawn((TestPos(5), ChildOf(mid), CustomParent4(mid.index_u32())))
            .id();
        let dead = world.spawn_empty().id();
        assert!(world.despawn(dead));

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: pos_h,
                component_id: pos_cid,
                mode: AccessMode::WRITE,
                uuid: TestPos::UUID,
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: sparse_h,
                component_id: sparse_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("hier::SparseVal"),
                size: 8,
                align: 8,
                storage_flags: BAM_STORAGE_FLAG_SPARSE_SET,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: p4_h,
                component_id: p4_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("hier::CustomParent4"),
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: pos_h,
                component_id: pos_cid,
                mode: AccessMode::FILTER_ONLY,
                uuid: TestPos::UUID,
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

        let mut out_ptr: *const c_void = core::ptr::null();
        let mut out_owner = FfiEntity::NULL;

        // SAFETY: Valid `iter` and stack outputs.
        unsafe {
            // 1. Default `ChildOf` (`BamComponent::INVALID`) walks `leaf -> mid -> root` on WRITE term 0.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(leaf.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(root.to_bits()));
            assert_eq!((*out_ptr.cast::<TestPos>()).0, 100);

            // 2. Explicit registered `ChildOf` handle via `ModRegistry` finds `SparseVal(777)` on `root`.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    child_of_h,
                    FfiEntity::from_bits(leaf.to_bits()),
                    1,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(root.to_bits()));
            assert_eq!((*out_ptr.cast::<SparseVal>()).0, 777);

            // 3. Custom 4-byte `EntityIndex` relation resolves via query plan.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    p4_h,
                    FfiEntity::from_bits(leaf.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(root.to_bits()));
            assert_eq!((*out_ptr.cast::<TestPos>()).0, 100);

            // 4. Root has no parent -> `NOT_FOUND`, outputs zeroed.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(root.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NOT_FOUND
            );
            assert!(out_ptr.is_null());
            assert_eq!(out_owner, FfiEntity::NULL);

            // 5. Filter-only term -> `PERMISSION_DENIED`; despawned entity -> `INVALID_ENTITY`.
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(leaf.to_bits()),
                    3,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::PERMISSION_DENIED
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(dead.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_ENTITY
            );
        }
    }

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    #[component(summary_tick)]
    struct SummaryParent8(FfiEntity);

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq, bevy_abi_mod_macros::ModComponent)]
    struct ByteNonRel(u8);

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq)]
    struct EmptyZeroZst;

    #[repr(C)]
    #[derive(
        bevy_ecs::prelude::Resource, Copy, Clone, Debug, bevy_abi_mod_macros::ModComponent,
    )]
    struct HierRes;

    #[repr(C)]
    #[derive(
        bevy_ecs::prelude::Resource, Copy, Clone, Debug, bevy_abi_mod_macros::ModComponent,
    )]
    struct HierRes2;

    #[repr(C)]
    #[derive(Component, Copy, Clone, Debug, PartialEq, bevy_abi_mod_macros::ModComponent)]
    struct CovParent8U64(u64);

    #[test]
    #[allow(clippy::cast_ptr_alignment)]
    fn sys_entity_children_guards_empty_targets_and_dynamic_missing_map() {
        use crate::ffi::iter::{QueryPlan, QueryTermMeta};
        use crate::system::HostSystemCtx;
        use crate::system::guest_system::RandomQueryView;
        use crate::system::query_group::MAX_SYSTEM_QUERIES;
        use bevy_abi_mod_sys::{BamIter, ComponentUuid};
        use bevy_ecs::change_detection::Tick;

        let mut world = World::new();
        let pos_cid = world.register_component::<TestPos>();
        let followers_cid = world.register_component::<CustomFollowers>();
        let dyn_zst_cid = world.register_component::<EmptyZeroZst>();

        let mut reg = ModRegistry::new();
        let pos_h = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        let followers_h = reg
            .register_relationship_target::<CustomFollowers>(
                "hier_cov::CustomFollowers",
                None,
                followers_cid,
            )
            .unwrap();

        // Register a dynamic component WITHOUT `DynamicComponentHookMap` in world yet.
        let mut dyn_h = BamComponent::INVALID;
        {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::new(AtomicBool::new(true)),
                &raw mut world,
                Some(ModId(12)),
            );
            let d = ComponentDesc::new(
                c"hier_cov::DynNotTarget".as_ptr(),
                ComponentUuid::from_parts("hier_cov::DynNotTarget", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and descriptor.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d, &mut dyn_h),
                    FfiResult::OK
                );
            }
            reg = ctx.registry.clone();
            ctx.finish_init();
        }
        let dyn_cid = reg.get_by_handle(dyn_h).unwrap().component_id;

        // Spawn entity with empty `CustomFollowers(vec![])` and `EmptyZeroZst`.
        let host_e = world
            .spawn((CustomFollowers(Vec::new()), EmptyZeroZst))
            .id();
        let host_e_ffi = FfiEntity::from_bits(host_e.to_bits());
        let dead = world.spawn_empty().id();
        assert!(world.despawn(dead));

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: pos_h,
                component_id: pos_cid,
                mode: AccessMode::READ,
                uuid: TestPos::UUID,
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: followers_h,
                component_id: followers_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_canonical_name("hier_cov::CustomFollowers"),
                size: core::mem::size_of::<CustomFollowers>(),
                align: core::mem::align_of::<CustomFollowers>(),
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: dyn_h,
                component_id: dyn_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_parts("hier_cov::DynNotTarget", 0, 1, 0),
                size: 0,
                align: 1,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let snap = reg.snapshot_arc();
        let iter_ptrs = [core::ptr::null_mut::<BamIter>(); MAX_SYSTEM_QUERIES];
        let mut views = [None; MAX_SYSTEM_QUERIES];
        views[0] = Some(RandomQueryView {
            plan: &plan,
            is_dense: true,
            matched_tables: &[],
            matched_archetypes: &[],
            has_query_state: true,
        });

        let mut unaligned_buf = [0u64; 2];
        let unaligned = (&raw mut unaligned_buf).cast::<u8>().wrapping_add(1);
        let mut out_ptr: *const FfiEntity = core::ptr::null();
        let mut out_len: usize = 0;

        // 1. `HostSystemCtx` WITHOUT world (`ctx.world == None`) returns `INVALID_STATE`.
        let mut ctx_no_world = HostSystemCtx::new(
            Arc::clone(&snap),
            None,
            String::new(),
            iter_ptrs,
            1,
            core::ptr::null_mut(),
        );
        // SAFETY: Valid `HostSystemCtx` and intentionally misaligned pointers tested via `is_aligned()`.
        unsafe {
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    core::ptr::null_mut(),
                    &raw mut out_len,
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    core::ptr::null_mut(),
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    unaligned.cast::<*const FfiEntity>(),
                    &raw mut out_len,
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    unaligned.cast::<usize>(),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    FfiEntity::NULL,
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    FfiEntity::from_bits(1u64 << 32),
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_no_world.as_ffi_ptr(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_STATE
            );
        }
        drop(ctx_no_world);

        // Poisoned `HostSystemCtx` -> `INVALID_STATE`
        let mut poisoned_ctx = HostSystemCtx::new(
            Arc::clone(&snap),
            None,
            String::new(),
            iter_ptrs,
            1,
            core::ptr::null_mut(),
        );
        poisoned_ctx.header.poison();
        let poisoned_ptr = poisoned_ctx.as_ffi_ptr();
        // SAFETY: Poisoned pointer is rejected by `validate_handle_ref` before dereference.
        unsafe {
            assert_eq!(
                host_sys_entity_children(
                    poisoned_ptr.cast_const(),
                    host_e_ffi,
                    BamComponent::INVALID,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_STATE
            );
        }

        // 2. Empty snapshot without `Children` -> `NOT_FOUND` for `BamComponent::INVALID`.
        {
            let empty_snap = ModRegistry::new().snapshot_arc();
            let mut ctx_empty = HostSystemCtx::new(
                empty_snap,
                None,
                String::new(),
                iter_ptrs,
                1,
                core::ptr::null_mut(),
            )
            .with_world_and_queries(
                world.as_unsafe_world_cell_readonly(),
                views,
                Tick::new(0),
                Tick::new(1),
            );
            // SAFETY: Valid world-backed `HostSystemCtx` and stack outputs.
            unsafe {
                assert_eq!(
                    host_sys_entity_children(
                        ctx_empty.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        BamComponent::INVALID,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::NOT_FOUND
                );
            }
        }

        // 3. World-backed `HostSystemCtx`: `PERMISSION_DENIED` (auto-registered `Children` not in query),
        // `INVALID_COMPONENT` (unknown target, non-target static, dynamic without hook_map, dynamic not a target),
        // `INVALID_ENTITY` (dead), empty `CustomFollowers` (`len == 0`), empty `DynamicTarget` (`len == 0`).
        {
            let mut ctx_w = HostSystemCtx::new(
                Arc::clone(&snap),
                None,
                String::new(),
                iter_ptrs,
                1,
                core::ptr::null_mut(),
            )
            .with_world_and_queries(
                world.as_unsafe_world_cell_readonly(),
                views,
                Tick::new(0),
                Tick::new(1),
            );
            // SAFETY: Valid world-backed `HostSystemCtx` and stack outputs.
            unsafe {
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        BamComponent::INVALID,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::PERMISSION_DENIED
                );
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        BamComponent::from_raw(999),
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::INVALID_COMPONENT
                );
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        FfiEntity::from_bits(dead.to_bits()),
                        followers_h,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::INVALID_ENTITY
                );
                // Non-relationship static component `TestPos` -> `INVALID_COMPONENT`
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        pos_h,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::INVALID_COMPONENT
                );
                // Empty `CustomFollowers` returns `OK` with `len == 0`.
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        followers_h,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::OK
                );
                assert_eq!(out_len, 0);
                // Dynamic component without `DynamicComponentHookMap` resource -> `INVALID_COMPONENT`
                assert_eq!(
                    host_sys_entity_children(
                        ctx_w.as_ffi_ptr().cast_const(),
                        host_e_ffi,
                        dyn_h,
                        &raw mut out_ptr,
                        &raw mut out_len,
                    ),
                    FfiResult::INVALID_COMPONENT
                );
            }
        }

        // Insert `DynamicComponentHookMap` and verify:
        // - `dyn_h` (not a registered relationship target) -> `INVALID_COMPONENT`
        // - a registered `DynamicTarget` with 0 children -> `OK`, `out_len == 0`
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut rel_h = BamComponent::INVALID;
        let mut tgt_h = BamComponent::INVALID;
        let mut bind_h = BamComponent::INVALID;
        let reg2 = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::new(AtomicBool::new(true)),
                &raw mut world,
                Some(ModId(13)),
            );
            let d_r = ComponentDesc::new(
                c"hier_cov::R".as_ptr(),
                ComponentUuid::from_parts("hier_cov::R", 8, 8, 0),
                8,
                8,
            );
            let d_t = ComponentDesc::new(
                c"hier_cov::T".as_ptr(),
                ComponentUuid::from_parts("hier_cov::T", 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and descriptors.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_r, &mut rel_h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_t, &mut tgt_h),
                    FfiResult::OK
                );
            }
            let bind_name =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", rel_h.0, tgt_h.0))
                    .unwrap();
            let d_b = ComponentDesc::new(
                bind_name.as_ptr(),
                ComponentUuid::from_parts(bind_name.to_str().unwrap(), 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and bind descriptor.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_b, &mut bind_h),
                    FfiResult::OK
                );
            }
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };
        let tgt_cid = reg2.get_by_handle(tgt_h).unwrap().component_id;
        let snap2 = reg2.snapshot_arc();
        let plan2 = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: dyn_h,
                component_id: dyn_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_parts("hier_cov::DynNotTarget", 0, 1, 0),
                size: 0,
                align: 1,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: tgt_h,
                component_id: tgt_cid,
                mode: AccessMode::READ,
                uuid: ComponentUuid::from_parts("hier_cov::T", 0, 1, 0),
                size: 0,
                align: 1,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);
        views[0] = Some(RandomQueryView {
            plan: &plan2,
            is_dense: true,
            matched_tables: &[],
            matched_archetypes: &[],
            has_query_state: true,
        });
        let mut ctx_w2 = HostSystemCtx::new(
            snap2,
            None,
            String::new(),
            iter_ptrs,
            1,
            core::ptr::null_mut(),
        )
        .with_world_and_queries(
            world.as_unsafe_world_cell_readonly(),
            views,
            Tick::new(0),
            Tick::new(1),
        );
        // SAFETY: Valid `HostSystemCtx` and stack outputs.
        unsafe {
            assert_eq!(
                host_sys_entity_children(
                    ctx_w2.as_ffi_ptr().cast_const(),
                    host_e_ffi,
                    dyn_h,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                host_sys_entity_children(
                    ctx_w2.as_ffi_ptr().cast_const(),
                    host_e_ffi,
                    tgt_h,
                    &raw mut out_ptr,
                    &raw mut out_len,
                ),
                FfiResult::OK
            );
            assert_eq!(out_len, 0);
        }
        let _ = dyn_zst_cid;
    }

    #[test]
    fn cascade_cache_sparse_dynamic_target_summary_tick_empty_and_parallel_strata() {
        use super::CascadeHierarchyCache;
        use bevy_abi_mod_sys::{BAM_COMPONENT_FLAG_SPARSE_SET, ComponentUuid};
        use bevy_ecs::component::ComponentId;
        use core::sync::atomic::{AtomicUsize, Ordering};

        // 1. Empty world: `ensure_built_child_of` returns `None`, `ensure_built` on unregistered /
        // non-relation (`u8`, non-target ZST) returns `true` then `false`.
        let mut world = World::new();
        let u8_cid = world.register_component::<ByteNonRel>();
        let zst_cid = world.register_component::<EmptyZeroZst>();
        let mut cache = CascadeHierarchyCache::new();
        assert_eq!(cache.ensure_built_child_of(&world), None);
        let missing_cid = ComponentId::new(999_999);
        assert!(cache.ensure_built(&world, missing_cid));
        assert!(!cache.ensure_built(&world, missing_cid));
        assert!(cache.ensure_built(&world, u8_cid));
        assert!(!cache.ensure_built(&world, u8_cid));
        assert!(cache.ensure_built(&world, zst_cid));
        assert!(!cache.ensure_built(&world, zst_cid));
        cache.for_each_cascade(missing_cid, |_, _, _| {});
        cache.par_for_each_stratum(missing_cid, |_, _| {});

        // Registered `ChildOf` with 0 entities (`all_nodes.is_empty()`), then with entities.
        let child_of_cid = world.register_component::<ChildOf>();
        assert_eq!(
            cache.ensure_built_child_of(&world),
            Some((child_of_cid, true))
        );
        assert_eq!(
            cache.ensure_built_child_of(&world),
            Some((child_of_cid, false))
        );

        // 2. `SummaryParent8` (`summary_tick`) + null `Entity8` + invalid `Entity4(u32::MAX)` +
        // `SparseParent8` self-cycle & removal & mutation.
        let sum_cid = world.register_component::<SummaryParent8>();
        let p8_cid = world.register_component::<CustomParent8>();
        let p4_cid = world.register_component::<CustomParent4>();
        let sp_cid = world.register_component::<SparseParent8>();

        let s_root = world.spawn_empty().id();
        let s_child = world
            .spawn(SummaryParent8(FfiEntity::from_bits(s_root.to_bits())))
            .id();
        world.spawn((CustomParent8(FfiEntity::NULL), CustomParent4(u32::MAX)));
        let sp_self = world.spawn_empty().id();
        world
            .entity_mut(sp_self)
            .insert(SparseParent8(FfiEntity::from_bits(sp_self.to_bits())));

        assert!(cache.ensure_built(&world, sum_cid));
        assert_eq!(cache.depth_of(sum_cid, s_child), Some(1));
        world.increment_change_tick();
        assert!(!cache.ensure_built(&world, sum_cid));
        // Mutating `SummaryParent8` updates table summary tick -> triggers rebuild.
        world
            .entity_mut(s_child)
            .get_mut::<SummaryParent8>()
            .unwrap()
            .0 = FfiEntity::from_bits(s_root.to_bits());
        assert!(cache.ensure_built(&world, sum_cid));

        assert!(cache.ensure_built(&world, p8_cid));
        assert!(cache.ensure_built(&world, p4_cid));
        assert!(cache.ensure_built(&world, sp_cid));
        assert!(cache.has_cycles(sp_cid));
        // Mutating `SparseParent8` in place triggers `get_changed_tick` rebuild.
        world
            .entity_mut(sp_self)
            .get_mut::<SparseParent8>()
            .unwrap()
            .0 = FfiEntity::from_bits(s_root.to_bits());
        assert!(cache.ensure_built(&world, sp_cid));
        assert!(!cache.has_cycles(sp_cid));
        // Despawn `sp_self` so `SparseSet` drops count to 0 (`len != cached_edge_count`).
        assert!(world.despawn(sp_self));
        assert!(cache.ensure_built(&world, sp_cid));

        // 3. `find_nearest_ancestor` hitting a cycle `cyc_a <-> cyc_b` returns `None`.
        let cyc_a = world.spawn_empty().id();
        let cyc_b = world.spawn_empty().id();
        world
            .entity_mut(cyc_a)
            .insert(CustomParent8(FfiEntity::from_bits(cyc_b.to_bits())));
        world
            .entity_mut(cyc_b)
            .insert(CustomParent8(FfiEntity::from_bits(cyc_a.to_bits())));
        assert!(cache.ensure_built(&world, p8_cid));
        assert_eq!(cache.find_nearest_ancestor(p8_cid, cyc_a, |_| false), None);

        // 4. `SparseSet` `DynamicTarget` (`BAM_COMPONENT_FLAG_SPARSE_SET`) fingerprint, rebuild,
        // self-loop/null child skip, and `iter_up_cascade_impl` `resolve_dynamic_target_parent`.
        ensure_dynamic_hook_map_in_world(&mut world);
        let mut s_rel_h = BamComponent::INVALID;
        let mut s_tgt_h = BamComponent::INVALID;
        let mut s_bind_h = BamComponent::INVALID;
        let reg_s = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                ModRegistry::new(),
                Arc::new(AtomicBool::new(true)),
                &raw mut world,
                Some(ModId(14)),
            );
            let d_r = ComponentDesc::new(
                c"hier_cov::SparseRel".as_ptr(),
                ComponentUuid::from_parts("hier_cov::SparseRel", 8, 8, 0),
                8,
                8,
            )
            .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET);
            let d_t = ComponentDesc::new(
                c"hier_cov::SparseTgt".as_ptr(),
                ComponentUuid::from_parts("hier_cov::SparseTgt", 0, 1, 0),
                0,
                1,
            )
            .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET);
            // SAFETY: Valid `LoadContext` and descriptors.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_r, &mut s_rel_h),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_t, &mut s_tgt_h),
                    FfiResult::OK
                );
            }
            let bname =
                std::ffi::CString::new(format!("__bam_rel_bind::{}::{}", s_rel_h.0, s_tgt_h.0))
                    .unwrap();
            let d_b = ComponentDesc::new(
                bname.as_ptr(),
                ComponentUuid::from_parts(bname.to_str().unwrap(), 0, 1, 0),
                0,
                1,
            );
            // SAFETY: Valid `LoadContext` and bind descriptor.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &d_b, &mut s_bind_h),
                    FfiResult::OK
                );
            }
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };
        let s_rel_cid = reg_s.get_by_handle(s_rel_h).unwrap().component_id;
        let s_tgt_cid = reg_s.get_by_handle(s_tgt_h).unwrap().component_id;
        let pos_cid = world.register_component::<TestPos>();
        let sun = world.spawn(TestPos(555)).id();
        let planet = world.spawn_empty().id();
        let mut sun_bits = sun.to_bits();
        // SAFETY: `s_rel_cid` has `(8, 8)` layout storing `FfiEntity`.
        unsafe {
            world.entity_mut(planet).insert_by_id(
                s_rel_cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut sun_bits).cast()),
            );
        }
        assert!(cache.ensure_built(&world, s_tgt_cid));
        assert!(!cache.ensure_built(&world, s_tgt_cid));
        assert_eq!(cache.parent_of(s_tgt_cid, planet), Some(sun));

        // Upward cascade along `SparseSet` `DynamicTarget` (`s_tgt_h`) finds `TestPos(555)` on `sun`
        // and returns `NOT_FOUND` when starting from `sun` (root).
        {
            use super::iter_up_cascade_impl;
            use crate::ffi::iter::{HostIter, QueryPlan, QueryTermMeta};
            use bevy_ecs::change_detection::Tick;

            world.insert_resource(reg_s);
            let plan = QueryPlan::from_terms(vec![QueryTermMeta {
                component: BamComponent::from_raw(1),
                component_id: pos_cid,
                mode: AccessMode::READ,
                uuid: TestPos::UUID,
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            }]);
            let mut iter = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly());
            let mut out_ptr: *const c_void = core::ptr::null();
            let mut out_owner = FfiEntity::NULL;
            // SAFETY: Valid `HostIter` and aligned stack outputs.
            unsafe {
                assert_eq!(
                    iter_up_cascade_impl(
                        iter.as_ffi_ptr(),
                        s_tgt_h,
                        FfiEntity::from_bits(planet.to_bits()),
                        0,
                        &raw mut out_ptr,
                        &raw mut out_owner,
                    ),
                    FfiResult::OK
                );
                assert_eq!(out_owner, FfiEntity::from_bits(sun.to_bits()));
                assert_eq!((*out_ptr.cast::<TestPos>()).0, 555);

                assert_eq!(
                    iter_up_cascade_impl(
                        iter.as_ffi_ptr(),
                        s_tgt_h,
                        FfiEntity::from_bits(sun.to_bits()),
                        0,
                        &raw mut out_ptr,
                        &raw mut out_owner,
                    ),
                    FfiResult::NOT_FOUND
                );
            }
        }

        // 5. `par_for_each_stratum` multi-grain parallel dispatch (`> 256` entities in non-Miri;
        // scaled down under Miri).
        let par_parent = world.spawn_empty().id();
        let fanout = if cfg!(miri) { 8usize } else { 300usize };
        for _ in 0..fanout {
            world.spawn(ChildOf(par_parent));
        }
        assert!(cache.ensure_built(&world, child_of_cid));
        let par_seen = AtomicUsize::new(0);
        cache.par_for_each_stratum(child_of_cid, |_, chunk| {
            par_seen.fetch_add(chunk.len(), Ordering::Relaxed);
        });
        assert_eq!(par_seen.load(Ordering::Relaxed), fanout + 1);
    }

    #[test]
    #[allow(clippy::cast_ptr_alignment)]
    fn iter_up_cascade_all_guards_and_deep_multi_stratum_brent_walk() {
        use super::iter_up_cascade_impl;
        use crate::ffi::iter::{HostIter, QueryPlan, QueryTermMeta};
        use bevy_abi_mod_sys::{BAM_STORAGE_FLAG_RESOURCE, ComponentUuid};
        use bevy_ecs::change_detection::Tick;

        let mut world = World::new();
        let pos_cid = world.register_component::<TestPos>();
        let res_cid = world.register_component::<HierRes>();
        let res2_cid = world.register_component::<HierRes2>();
        let byte_cid = world.register_component::<ByteNonRel>();
        let p8_cid = world.register_component::<CovParent8U64>();

        let mut reg = ModRegistry::new();
        let pos_h = reg
            .register_component::<TestPos>(pos_cid, Default::default())
            .unwrap();
        let res_h = reg.register_resource::<HierRes>(res_cid).unwrap();
        let res2_h = reg.register_resource::<HierRes2>(res2_cid).unwrap();
        let byte_h = reg
            .register_component::<ByteNonRel>(byte_cid, Default::default())
            .unwrap();
        let p8_h = reg
            .register_component::<CovParent8U64>(p8_cid, Default::default())
            .unwrap();

        // Deep 7-node chain: `n0(TestPos(42)) <- n1 <- n2 <- n3 <- n4 <- n5 <- n6`.
        let n0 = world.spawn(TestPos(42)).id();
        let mut curr = n0;
        for _ in 0..6 {
            curr = world.spawn(ChildOf(curr)).id();
        }
        let n6 = curr;

        // Entity pointing to a despawned parent via `CovParent8U64` -> hits `world.get_entity(curr).is_err()`.
        let dead_parent = world.spawn_empty().id();
        assert!(world.despawn(dead_parent));
        let orphan = world.spawn(CovParent8U64(dead_parent.to_bits())).id();

        let plan = QueryPlan::from_terms(vec![
            QueryTermMeta {
                component: pos_h,
                component_id: pos_cid,
                mode: AccessMode::READ,
                uuid: TestPos::UUID,
                size: 4,
                align: 4,
                storage_flags: 0,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
            QueryTermMeta {
                component: res_h,
                component_id: res_cid,
                mode: AccessMode::RESOURCE_READ,
                uuid: ComponentUuid::from_canonical_name("hier_cov::HierRes"),
                size: 0,
                align: 1,
                storage_flags: BAM_STORAGE_FLAG_RESOURCE,
                msg_read_batch_fn: None,
                msg_send_fn: None,
                msg_write_batch_fn: None,
            },
        ]);

        let mut unaligned_buf = [0u64; 2];
        let unaligned = (&raw mut unaligned_buf).cast::<u8>().wrapping_add(1);
        let mut out_ptr: *const c_void = core::ptr::null();
        let mut out_owner = FfiEntity::NULL;

        // 1. Guards without `world` on `HostIter` and without `ModRegistry` in `World`.
        let mut iter_no_world = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        );
        // SAFETY: Valid stack buffers and intentionally misaligned/null pointers checked upfront.
        unsafe {
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    core::ptr::null_mut(),
                    &raw mut out_owner,
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    core::ptr::null_mut(),
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    unaligned.cast::<*const c_void>(),
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    unaligned.cast::<FfiEntity>(),
                ),
                FfiResult::INVALID_ARGUMENT
            );
            assert_eq!(
                iter_up_cascade_impl(
                    core::ptr::null_mut(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NULL_POINTER
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::NULL,
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(1u64 << 32),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(Entity::PLACEHOLDER.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_ENTITY
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    99,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OUT_OF_RANGE
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_no_world.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_STATE
            );
        }
        drop(iter_no_world);

        // 2. World without `ModRegistry` -> `INVALID_COMPONENT` for explicit relation not in plan;
        // resource handle in query plan (`res_h`) -> `INVALID_COMPONENT`.
        {
            let mut iter_no_reg = HostIter::new_dense(
                &world.storages().tables,
                &[],
                &plan,
                Tick::new(0),
                Tick::new(1),
            )
            .with_world(world.as_unsafe_world_cell_readonly());
            // SAFETY: Valid `HostIter` and stack outputs.
            unsafe {
                assert_eq!(
                    iter_up_cascade_impl(
                        iter_no_reg.as_ffi_ptr(),
                        res_h,
                        FfiEntity::from_bits(n6.to_bits()),
                        0,
                        &raw mut out_ptr,
                        &raw mut out_owner,
                    ),
                    FfiResult::INVALID_COMPONENT
                );
                assert_eq!(
                    iter_up_cascade_impl(
                        iter_no_reg.as_ffi_ptr(),
                        p8_h,
                        FfiEntity::from_bits(n6.to_bits()),
                        0,
                        &raw mut out_ptr,
                        &raw mut out_owner,
                    ),
                    FfiResult::INVALID_COMPONENT
                );
            }
        }

        // 3. World WITH `ModRegistry`: unknown relation, resource relation, non-relation `u8` layout,
        // orphan pointing at despawned entity (`NOT_FOUND`), and 6-hop multi-stratum `ChildOf` walk.
        world.insert_resource(reg);
        let mut iter_live = HostIter::new_dense(
            &world.storages().tables,
            &[],
            &plan,
            Tick::new(0),
            Tick::new(1),
        )
        .with_world(world.as_unsafe_world_cell_readonly());
        // SAFETY: Valid `HostIter` and stack outputs.
        unsafe {
            assert_eq!(
                iter_up_cascade_impl(
                    iter_live.as_ffi_ptr(),
                    BamComponent::from_raw(999),
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_live.as_ffi_ptr(),
                    res2_h,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_live.as_ffi_ptr(),
                    byte_h,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::INVALID_COMPONENT
            );
            assert_eq!(
                iter_up_cascade_impl(
                    iter_live.as_ffi_ptr(),
                    p8_h,
                    FfiEntity::from_bits(orphan.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::NOT_FOUND
            );
            // 6-hop multi-stratum walk `n6 -> n5 -> n4 -> n3 -> n2 -> n1 -> n0` exercises Brent's doubling.
            assert_eq!(
                iter_up_cascade_impl(
                    iter_live.as_ffi_ptr(),
                    BamComponent::INVALID,
                    FfiEntity::from_bits(n6.to_bits()),
                    0,
                    &raw mut out_ptr,
                    &raw mut out_owner,
                ),
                FfiResult::OK
            );
            assert_eq!(out_owner, FfiEntity::from_bits(n0.to_bits()));
            assert_eq!((*out_ptr.cast::<TestPos>()).0, 42);
        }
    }
}
