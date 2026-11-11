//! Per-query-group descriptor ownership and incremental archetype/table match caching.

use crate::ffi::iter::QueryColumnDesc;
use bevy_abi_mod_sys::ComponentAccessMode;
use bevy_ecs::archetype::{ArchetypeGeneration, ArchetypeId};
use bevy_ecs::component::ComponentId;
use bevy_ecs::query::QueryState;
use bevy_ecs::storage::TableId;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;

/// Maximum number of concurrent/nested queries supported per multi-query dynamic system.
pub const MAX_SYSTEM_QUERIES: usize = 8;

/// Owned representation of component access mode and component name for a dynamic query clause.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComponentAccessDescOwned {
    /// Canonical component or resource name.
    pub name: String,
    /// Requested access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`, `RESOURCE_READ`, etc.).
    pub mode: ComponentAccessMode,
}

pub(crate) struct CachedQueryGroup {
    pub(crate) descs: Vec<ComponentAccessDescOwned>,
    pub(crate) columns: Vec<QueryColumnDesc>,
    pub(crate) declared_resources: Vec<(ComponentId, bool)>,
    pub(crate) query_state: Option<QueryState<(), ()>>,
    pub(crate) is_dense: bool,
    pub(crate) sparse_column_mask: u32,
    pub(crate) archetype_generation: ArchetypeGeneration,
    pub(crate) matched_tables: Vec<TableId>,
    pub(crate) matched_table_bits: Vec<bool>,
    pub(crate) matched_archetypes: Vec<ArchetypeId>,
}

impl CachedQueryGroup {
    pub(crate) fn new(descs: Vec<ComponentAccessDescOwned>) -> Self {
        Self {
            descs,
            columns: Vec::new(),
            declared_resources: Vec::new(),
            query_state: None,
            is_dense: true,
            sparse_column_mask: 0,
            archetype_generation: ArchetypeGeneration::initial(),
            matched_tables: Vec::new(),
            matched_table_bits: Vec::new(),
            matched_archetypes: Vec::new(),
        }
    }

    pub(crate) fn reset_cache(&mut self) {
        self.archetype_generation = ArchetypeGeneration::initial();
        self.matched_tables.clear();
        self.matched_table_bits.clear();
        self.matched_archetypes.clear();
    }

    pub(crate) fn update_archetypes(&mut self, world: UnsafeWorldCell<'_>) {
        let archetypes = world.archetypes();
        let new_gen = archetypes.generation();
        if self.archetype_generation == new_gen {
            return;
        }
        let old_gen = core::mem::replace(&mut self.archetype_generation, new_gen);

        let Some(ref mut state) = self.query_state else {
            return;
        };
        state.update_archetypes_unsafe_world_cell(world);

        for archetype in &archetypes[old_gen..] {
            if state.matches_component_set(&|id| archetype.contains(id)) {
                if self.is_dense {
                    let table_id = archetype.table_id();
                    let idx = table_id.as_usize();
                    if idx >= self.matched_table_bits.len() {
                        self.matched_table_bits.resize(idx + 1, false);
                    }
                    if !self.matched_table_bits[idx] {
                        self.matched_table_bits[idx] = true;
                        self.matched_tables.push(table_id);
                    }
                } else {
                    self.matched_archetypes.push(archetype.id());
                }
            }
        }
    }
}
