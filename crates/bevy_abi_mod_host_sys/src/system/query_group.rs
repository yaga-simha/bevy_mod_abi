//! Per-query-group descriptor ownership and `QueryState`-backed archetype/table caching (`P-09`).

use crate::ffi::iter::{QueryPlan, SparseRowScratch};
use bevy_abi_mod_sys::{BamTermDesc, MAX_QUERIES_PER_SYSTEM};
use bevy_ecs::archetype::{ArchetypeGeneration, ArchetypeId};
use bevy_ecs::query::QueryState;
use bevy_ecs::storage::TableId;
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;

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

pub(crate) struct CachedQueryGroup {
    pub(crate) spec: BamQuerySpecOwned,
    pub(crate) plan: QueryPlan,
    pub(crate) sparse_scratch: SparseRowScratch,
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
            query_state: None,
            is_dense: true,
            archetype_generation: ArchetypeGeneration::initial(),
            matched_tables: Vec::new(),
            matched_archetypes: Vec::new(),
        }
    }

    pub(crate) fn reset_cache(&mut self) {
        self.archetype_generation = ArchetypeGeneration::initial();
        self.matched_tables.clear();
        self.matched_archetypes.clear();
    }

    /// Incrementally updates `QueryState` against newly created archetypes and copies matched
    /// table/archetype IDs directly from Bevy's `FixedBitSet` cache (`P-09`).
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
