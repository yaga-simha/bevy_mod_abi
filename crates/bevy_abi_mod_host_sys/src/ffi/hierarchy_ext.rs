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
    ffi::register::DynamicComponentHookMap,
    handle_header::{
        decode_live_entity, ffi_try, init_out_ptr, validate_iter_ref, validate_sys_ctx_ref,
    },
    registry::ModRegistry,
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
/// - `sys_ctx` must point to an active [`crate::HostSystemCtx`] cast to [`BamSystemCtx`], or null.
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
    // SAFETY: `out_entities` and `out_len` are checked for null/alignment before writing defaults.
    ffi_try!(unsafe { init_out_ptr(out_entities, core::ptr::null()) });
    // SAFETY: Same as above for `out_len`.
    ffi_try!(unsafe { init_out_ptr(out_len, 0) });
    // SAFETY: Validates `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let ctx = ffi_try!(unsafe { validate_sys_ctx_ref(sys_ctx) });
    let bevy_entity = ffi_try!(decode_live_entity(entity));

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
    fn reset_empty(&mut self, this_run: Tick) -> bool {
        if self.built && self.strata.is_empty() {
            self.last_checked_tick = this_run;
            return false;
        }
        self.parent_map.clear();
        self.depth_map.clear();
        self.strata.clear();
        self.cached_edge_count = 0;
        self.cached_dynamic_hash = 0;
        self.has_cycles = false;
        self.built = true;
        self.last_checked_tick = this_run;
        self.rebuild_count += 1;
        true
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
            return entry.reset_empty(this_run);
        };

        let storage_type = comp_info.storage_type();
        let Some(layout) = classify_relation_layout(
            world.as_unsafe_world_cell_readonly(),
            relation,
            comp_info.layout().size(),
        ) else {
            return entry.reset_empty(this_run);
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
        world.init_resource::<Self>();
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
    // SAFETY: `out_ptr` and `out_owner` are checked for null/alignment before writing defaults.
    ffi_try!(unsafe { init_out_ptr(out_ptr, core::ptr::null()) });
    // SAFETY: Same as above for `out_owner`.
    ffi_try!(unsafe { init_out_ptr(out_owner, FfiEntity::NULL) });

    // SAFETY: Validates `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_ref(iter.cast_const()) });
    let bevy_entity = ffi_try!(decode_live_entity(entity));

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
#[path = "../../tests/unit/hierarchy_ext_tests.rs"]
mod tests;
