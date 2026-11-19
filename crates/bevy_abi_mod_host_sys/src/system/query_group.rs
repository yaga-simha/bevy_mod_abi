//! Per-query-group descriptor ownership and `QueryState`-backed archetype/table caching.

use crate::ffi::iter::{QueryPlan, SparseRowScratch};
use bevy_abi_mod_sys::{BamTermDesc, ComponentUuid, MAX_QUERIES_PER_SYSTEM};
use bevy_ecs::archetype::{ArchetypeGeneration, ArchetypeId};
use bevy_ecs::prelude::Resource;
use bevy_ecs::query::QueryState;
use bevy_ecs::storage::TableId;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_platform::collections::HashMap;
use core::any::Any;
use std::sync::{Arc, Mutex};

/// Maximum number of concurrent/nested queries supported per [`crate::GuestSystem`] (`8`).
pub const MAX_SYSTEM_QUERIES: usize = MAX_QUERIES_PER_SYSTEM;

/// Owned representation of a validated [`bevy_abi_mod_sys::BamQueryDesc`] clause.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct BamQuerySpecOwned {
    /// Validated `BamQueryDesc::flags` (`0` in ABI `0.2`).
    pub flags: u32,
    /// Validated handle-based query/resource terms (`0..=MAX_TERMS_PER_QUERY`).
    pub terms: Vec<BamTermDesc>,
}

impl BamQuerySpecOwned {
    /// Creates a query clause specification from `terms` with `flags = 0`.
    #[inline]
    pub fn new(terms: Vec<BamTermDesc>) -> Self {
        Self { flags: 0, terms }
    }
}

/// Lookup key identifying a persistent message or removed-component cursor across hot reloads.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct PersistentCursorKey {
    pub(crate) mod_namespace: Option<Arc<str>>,
    pub(crate) system_name: Arc<str>,
    pub(crate) query_idx: usize,
    pub(crate) term_idx: usize,
    pub(crate) component_uuid: ComponentUuid,
}

type CursorStoreMap = HashMap<PersistentCursorKey, Box<dyn Any + Send + Sync>>;

/// World resource preserving opaque `MessageCursor<M>` and `MessageCursor<RemovedComponentEntity>`
/// offsets across mod hot-reloads so re-registered systems resume reading without duplicating or
/// skipping queued messages.
#[derive(Resource, Default, Clone)]
pub struct PersistentMessageCursors {
    store: Arc<Mutex<CursorStoreMap>>,
}

impl PersistentMessageCursors {
    /// Returns a cloned handle to the shared thread-safe cursor store.
    #[inline]
    pub(crate) fn shared_store(&self) -> Arc<Mutex<CursorStoreMap>> {
        Arc::clone(&self.store)
    }

    /// Removes and returns a previously saved cursor matching `key`, if present.
    #[inline]
    pub(crate) fn take_from_store(
        store: &Arc<Mutex<CursorStoreMap>>,
        key: &PersistentCursorKey,
    ) -> Option<Box<dyn Any + Send + Sync>> {
        store.lock().ok()?.remove(key)
    }
}

pub(crate) struct CachedQueryGroup {
    pub(crate) spec: BamQuerySpecOwned,
    pub(crate) plan: QueryPlan,
    pub(crate) sparse_scratch: SparseRowScratch,
    pub(crate) msg_cursors: Vec<Option<Box<dyn Any + Send + Sync>>>,
    pub(crate) cursor_uuids: Vec<Option<ComponentUuid>>,
    pub(crate) owner_system_key: Option<(Option<Arc<str>>, Arc<str>)>,
    pub(crate) persistent_store: Option<(Arc<Mutex<CursorStoreMap>>, usize)>,
    pub(crate) query_state: Option<QueryState<(), ()>>,
    pub(crate) is_dense: bool,
    pub(crate) archetype_generation: ArchetypeGeneration,
    pub(crate) matched_tables: Vec<TableId>,
    pub(crate) matched_archetypes: Vec<ArchetypeId>,
}

impl CachedQueryGroup {
    pub(crate) fn new(spec: BamQuerySpecOwned) -> Self {
        Self {
            spec,
            plan: QueryPlan::default(),
            sparse_scratch: SparseRowScratch::default(),
            msg_cursors: Vec::new(),
            cursor_uuids: Vec::new(),
            owner_system_key: None,
            persistent_store: None,
            query_state: None,
            is_dense: true,
            archetype_generation: ArchetypeGeneration::initial(),
            matched_tables: Vec::new(),
            matched_archetypes: Vec::new(),
        }
    }

    /// Records the owning `(mod_namespace, system_name)` identity so any active message or
    /// removed-component cursors are saved into [`PersistentMessageCursors`] when dropped and
    /// restored when re-initialized after a hot reload.
    pub(crate) fn set_system_identity(
        &mut self,
        mod_namespace: Option<Arc<str>>,
        system_name: impl Into<Arc<str>>,
    ) {
        self.owner_system_key = Some((mod_namespace, system_name.into()));
    }

    pub(crate) fn reset_cache(&mut self) {
        self.archetype_generation = ArchetypeGeneration::initial();
        self.matched_tables.clear();
        self.matched_archetypes.clear();
    }

    /// Incrementally updates `QueryState` against newly created archetypes and copies matched
    /// table/archetype IDs directly from Bevy's `FixedBitSet` cache.
    pub(crate) fn update_archetypes(&mut self, world: UnsafeWorldCell<'_>) {
        let new_gen = world.archetypes().generation();
        if self.archetype_generation == new_gen {
            return;
        }
        self.archetype_generation = new_gen;

        let Some(ref mut state) = self.query_state else {
            return;
        };
        state.update_archetypes_unsafe_world_cell(world);

        if self.is_dense {
            self.matched_tables.clear();
            self.matched_tables.extend(state.matched_tables());
        } else {
            self.matched_archetypes.clear();
            self.matched_archetypes.extend(state.matched_archetypes());
        }
    }
}

impl Drop for CachedQueryGroup {
    fn drop(&mut self) {
        let Some((ref store, query_idx)) = self.persistent_store else {
            return;
        };
        let Some((ref mod_namespace, ref system_name)) = self.owner_system_key else {
            return;
        };
        if self.msg_cursors.is_empty() {
            return;
        }
        let Ok(mut map) = store.lock() else {
            return;
        };
        for (term_idx, cursor_slot) in self.msg_cursors.iter_mut().enumerate() {
            let Some(cursor) = cursor_slot.take() else {
                continue;
            };
            let Some(component_uuid) = self.cursor_uuids.get(term_idx).copied().flatten() else {
                continue;
            };
            map.insert(
                PersistentCursorKey {
                    mod_namespace: mod_namespace.clone(),
                    system_name: Arc::clone(system_name),
                    query_idx,
                    term_idx,
                    component_uuid,
                },
                cursor,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;

    #[test]
    fn cached_query_group_drop_guards_and_empty_update_archetypes() {
        let world = World::new();
        let store = PersistentMessageCursors::default().shared_store();

        // 1. `persistent_store` present, `owner_system_key == None` -> drop skips persistence.
        {
            let mut g = CachedQueryGroup::new(BamQuerySpecOwned::default());
            g.persistent_store = Some((Arc::clone(&store), 0));
            g.msg_cursors.push(Some(Box::new(1u32)));
            g.update_archetypes(world.as_unsafe_world_cell_readonly());
            g.reset_cache();
        }
        assert!(store.lock().unwrap().is_empty());

        // 2. `owner_system_key` present, `cursor_uuids` has `None` -> drop skips `None` UUID entry.
        {
            let mut g = CachedQueryGroup::new(BamQuerySpecOwned::default());
            g.set_system_identity(Some(Arc::from("mod_qg")), "sys_qg");
            g.persistent_store = Some((Arc::clone(&store), 0));
            g.msg_cursors.push(None);
            g.msg_cursors.push(Some(Box::new(2u32)));
            g.cursor_uuids = vec![None, None];
        }
        assert!(store.lock().unwrap().is_empty());
    }
}
