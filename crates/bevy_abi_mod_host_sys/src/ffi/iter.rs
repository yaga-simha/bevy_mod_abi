//! Dense chunk and indirect sparse-row query iterator (`HostIter`)
//!.
//!
//! # Intent
//! Serializing ECS archetype tables across an FFI boundary on every frame destroys cache
//! locality and frame budgets. `HostIter` exposes Bevy's underlying `Table` columns directly
//! as contiguous Structure-of-Arrays (SoA) byte slices—and `SparseSet` columns as batched
//! per-row pointer arrays—without copying component payloads.
//!
//! # How It Works
//! 1. **Pre-resolved Metadata:** At system registration time, component handles are resolved
//!    once into [`QueryTermMeta`] entries containing [`ComponentId`], byte size/alignment,
//!    and storage flags.
//! 2. **Archetype Planning:** [`QueryPlan`] caches matching [`ArchetypeId`]s and table
//!    column indices so per-frame iteration avoids string hashing or repeated archetype scans.
//! 3. **Borrow-Checked `UnsafeWorldCell` Access:** [`HostIter`] holds an `UnsafeWorldCell`
//!    scoped to the duration of [`GuestSystem`](crate::system::GuestSystem) execution. Because
//!    `GuestSystem` registers all read/write [`ComponentId`]s in Bevy's `FilteredAccessSet`,
//!    Bevy's scheduler guarantees that no conflicting system runs concurrently.
//! 4. **Change Detection:** Mutable column accessors stamp Bevy's `changed` ticks with
//!    `this_run`, while [`TickFilterTerm`] evaluates `Added<T>` / `Changed<T>` predicates
//!    against `last_run` and `this_run`.

use crate::handle_header::{HandleHeader, ITER_MAGIC, validate_handle_mut, validate_handle_ref};
use crate::registry::{MsgReadBatchFn, MsgSendFn, MsgWriteBatchFn};
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use bevy_abi_mod_sys::{
    AccessMode, BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BamChangeTicks, BamChunkTaskFn, BamColumnInfo, BamComponent,
    BamIter, ComponentUuid, FfiBool, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY,
};
use bevy_ecs::archetype::{ArchetypeId, Archetypes};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::storage::{SparseSets, Table, TableId, TableRow, Tables};
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use bevy_tasks::{ComputeTaskPool, TaskPool};
use core::any::Any;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use nonmax::NonMaxU32;

// Compile-time layout assertions guarding zero-copy `Entity` -> `FfiEntity` slice casts.
const _: () = assert!(core::mem::size_of::<Entity>() == core::mem::size_of::<FfiEntity>());
const _: () = assert!(core::mem::align_of::<Entity>() == core::mem::align_of::<FfiEntity>());
const _: () = assert!(core::mem::size_of::<*mut u8>() == core::mem::size_of::<*const u8>());

/// Pre-resolved metadata for one term inside a `BamQueryDesc` (`0..MAX_TERMS_PER_QUERY`).
#[derive(Clone, Copy, Debug)]
pub struct QueryTermMeta {
    /// 1-based [`BamComponent`] handle.
    pub component: BamComponent,
    /// Bevy [`ComponentId`] assigned in the host `World`.
    pub component_id: ComponentId,
    /// Declared [`AccessMode`].
    pub mode: AccessMode,
    /// Registered 128-bit [`ComponentUuid`].
    pub uuid: ComponentUuid,
    /// Byte size (`size_of::<T>()`).
    pub size: usize,
    /// Byte alignment (`align_of::<T>()`).
    pub align: usize,
    /// Bitmask of `BAM_STORAGE_FLAG_*`.
    pub storage_flags: u32,
    /// Optional monomorphized batch message reader.
    pub msg_read_batch_fn: Option<MsgReadBatchFn>,
    /// Optional monomorphized message sender.
    pub msg_send_fn: Option<MsgSendFn>,
    /// Optional monomorphized batch message writer.
    pub msg_write_batch_fn: Option<MsgWriteBatchFn>,
}

impl QueryTermMeta {
    /// Returns `true` if this term uses `StorageType::SparseSet`.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if this term is immutable (`BAM_STORAGE_FLAG_IMMUTABLE`).
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0
    }

    /// Returns `true` if this term refers to a singleton Bevy `Resource`.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if this term refers to a buffered Bevy `Messages<M>` stream.
    #[inline]
    pub const fn is_message(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_MESSAGE) != 0
    }
}

/// Precomputed tick-filter term (`Changed<T>` or `Added<T>`) resolved at `GuestSystem::initialize`.
#[derive(Clone, Copy, Debug)]
pub struct TickFilterTerm {
    /// Target Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// `true` for `AccessMode::ADDED`, `false` for `AccessMode::CHANGED`.
    pub is_added: bool,
    /// `true` if stored in `SparseSets` rather than `Table`.
    pub is_sparse: bool,
}

/// Precomputed filter clause inside a disjunctive `Or<(...)>` group (`or_group != 0`).
#[derive(Clone, Copy, Debug)]
pub struct OrFilterClause {
    /// Target Bevy [`ComponentId`].
    pub component_id: ComponentId,
    /// Filter access mode (`FILTER_ONLY`, `WITHOUT`, `CHANGED`, or `ADDED`).
    pub mode: AccessMode,
    /// `true` if stored in `SparseSets` rather than `Table`.
    pub is_sparse: bool,
}

/// Disjunctive `Or<(...)>` filter group (`group_id != 0`) composed of [`OrFilterClause`]s.
#[derive(Clone, Debug)]
pub struct OrFilterGroup {
    /// Non-zero group identifier (`1..=255`).
    pub group_id: u8,
    /// Member clauses combined via logical OR.
    pub terms: Vec<OrFilterClause>,
}

/// Precomputed static layout and term plan for a `BamQueryDesc`.
#[derive(Clone, Debug, Default)]
pub struct QueryPlan {
    /// Term metadata in declaration order (`0..term_count`).
    pub terms: Vec<QueryTermMeta>,
    /// Precomputed indices (`0..term_count`) of dense `Table` data columns.
    pub dense_data_indices: Vec<u8>,
    /// Precomputed indices (`0..term_count`) of `AccessMode::HAS` presence indicator terms.
    pub has_indices: Vec<u8>,
    /// `true` when at least one `AccessMode::HAS` term is backed by `StorageType::SparseSet`.
    pub has_sparse_term: bool,
    /// Bitmask of `term_index`es backed by `StorageType::SparseSet` with entity column data.
    pub sparse_data_mask: u32,
    /// Bitmask of `term_index`es backed by `StorageType::Table` with entity column data.
    pub dense_data_mask: u32,
    /// Bitmask of `term_index`es assigned to a non-zero `or_group`.
    pub or_term_mask: u32,
    /// Precomputed top-level (`or_group == 0`) `Changed<T>` / `Added<T>` filter terms.
    pub tick_filters: Vec<TickFilterTerm>,
    /// Precomputed disjunctive `Or<(...)>` filter groups (`or_group != 0`).
    pub or_groups: Vec<OrFilterGroup>,
    /// `true` when `terms.is_empty()` or at least one term is an entity term
    /// (`!is_resource() && !is_message() && !is_removed_read()`).
    pub has_entity_terms: bool,
}

impl QueryPlan {
    /// Builds a [`QueryPlan`] directly from a slice of [`QueryTermMeta`]s with all `or_group = 0`
    /// (useful for unit tests as well as `GuestSystem::initialize`).
    pub fn from_terms(terms: Vec<QueryTermMeta>) -> Self {
        Self::from_terms_with_or_groups(terms, &[])
    }

    /// Builds a [`QueryPlan`] from [`QueryTermMeta`]s and parallel per-term `or_group` IDs.
    pub fn from_terms_with_or_groups(terms: Vec<QueryTermMeta>, term_or_groups: &[u8]) -> Self {
        let mut dense_data_indices = Vec::new();
        let mut has_indices = Vec::new();
        let mut has_sparse_term = false;
        let mut sparse_data_mask = 0u32;
        let mut dense_data_mask = 0u32;
        let mut or_term_mask = 0u32;
        let mut tick_filters = Vec::new();
        let mut or_groups: Vec<OrFilterGroup> = Vec::new();
        let has_entity_terms = terms.is_empty()
            || terms.iter().any(|t| {
                !t.mode.is_resource() && !t.mode.is_message() && !t.mode.is_removed_read()
            });

        for (i, t) in terms.iter().enumerate().take(MAX_TERMS_PER_QUERY) {
            let or_group = term_or_groups.get(i).copied().unwrap_or(0);
            if or_group != 0 {
                or_term_mask |= 1u32 << i;
                let clause = OrFilterClause {
                    component_id: t.component_id,
                    mode: t.mode,
                    is_sparse: t.is_sparse_set(),
                };
                if let Some(group) = or_groups.iter_mut().find(|g| g.group_id == or_group) {
                    group.terms.push(clause);
                } else {
                    or_groups.push(OrFilterGroup {
                        group_id: or_group,
                        terms: vec![clause],
                    });
                }
                continue;
            }

            if t.mode.is_has() && !t.is_resource() && !t.is_message() {
                has_indices.push(i as u8);
                if t.is_sparse_set() {
                    has_sparse_term = true;
                }
                continue;
            }

            if t.mode.is_tick_filter() {
                tick_filters.push(TickFilterTerm {
                    component_id: t.component_id,
                    is_added: t.mode == AccessMode::ADDED,
                    is_sparse: t.is_sparse_set(),
                });
            }

            if !t.mode.has_entity_column_data() || t.is_resource() || t.is_message() {
                continue;
            }

            // If a tick filter (`Changed<T>` / `Added<T>`) shares a `component_id` with a
            // `WRITE` / `OPTIONAL_WRITE` term in the same query, suppress column data on the
            // read-filter term so both cannot alias `&mut T` and `&T` simultaneously.
            if t.mode.is_tick_filter()
                && terms.iter().enumerate().any(|(j, other)| {
                    term_or_groups.get(j).copied().unwrap_or(0) == 0
                        && other.component_id == t.component_id
                        && (other.mode == AccessMode::WRITE
                            || other.mode == AccessMode::OPTIONAL_WRITE)
                })
            {
                continue;
            }

            if t.is_sparse_set() {
                sparse_data_mask |= 1u32 << i;
            } else {
                dense_data_mask |= 1u32 << i;
                dense_data_indices.push(i as u8);
            }
        }

        Self {
            terms,
            dense_data_indices,
            has_indices,
            has_sparse_term,
            sparse_data_mask,
            dense_data_mask,
            or_term_mask,
            tick_filters,
            or_groups,
            has_entity_terms,
        }
    }
}

/// Per-query-group reusable scratch storage for indirect sparse-set row pointer batching.
#[derive(Default)]
pub struct SparseRowScratch {
    entities: Vec<FfiEntity>,
    row_ptrs: Vec<Vec<*mut u8>>,
    tick_ptrs: Vec<Vec<*const UnsafeCell<Tick>>>,
    summary_ptrs: Vec<Option<*const AtomicTick>>,
}

// SAFETY: `SparseRowScratch` holds temporary pointers only during an active single-threaded
// `GuestSystem::run_unsafe` chunk; all slices are cleared/overwritten before dereferencing and
// never accessed across threads.
unsafe impl Send for SparseRowScratch {}
// SAFETY: Shared references to `GuestSystem` never read or dereference `SparseRowScratch`.
unsafe impl Sync for SparseRowScratch {}

impl SparseRowScratch {
    /// Ensures inner per-term vectors cover `term_count` terms.
    pub fn ensure_terms(&mut self, term_count: usize) {
        if self.row_ptrs.len() < term_count {
            self.row_ptrs.resize_with(term_count, Vec::new);
            self.tick_ptrs.resize_with(term_count, Vec::new);
            self.summary_ptrs.resize(term_count, None);
        }
    }
}

#[derive(Clone, Copy)]
struct ActiveDenseColumn<'w> {
    data_ptr: *mut u8,
    changed_ticks: &'w [UnsafeCell<Tick>],
    summary_tick: Option<&'w AtomicTick>,
}

enum IterBacking<'w> {
    ResourceOnly,
    DenseTables {
        matched_tables: &'w [TableId],
        table_cursor: usize,
        row_cursor: usize,
        single_table_end_row: Option<usize>,
    },
    Archetypes {
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        archetype_cursor: usize,
        entity_cursor: usize,
        single_arch_end_entity: Option<usize>,
    },
}

struct DenseTableFilters<'t> {
    skip: bool,
    filter_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY],
    filter_len: usize,
    or_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY],
    or_slice_len: usize,
    or_ranges: [(usize, usize); MAX_TERMS_PER_QUERY],
    or_group_len: usize,
}

struct ArchetypeFilters<'t> {
    skip: bool,
    dense_filter_slices: [(usize, &'t [UnsafeCell<Tick>]); MAX_TERMS_PER_QUERY],
    dense_filter_len: usize,
    or_dense_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY],
    or_dense_len: usize,
    or_sparse_clauses: [(ComponentId, bool); MAX_TERMS_PER_QUERY],
    or_sparse_len: usize,
    or_ranges: [(usize, usize, usize, usize); MAX_TERMS_PER_QUERY],
    or_group_len: usize,
}

/// Concrete `#[repr(C)]` host chunk and resource iterator backing opaque [`BamIter`].
#[repr(C)]
pub struct HostIter<'w> {
    /// Leading handle validation header (`ITER_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    tables: Option<&'w Tables>,
    backing: IterBacking<'w>,
    pub(crate) plan: &'w QueryPlan,
    last_run: Tick,
    this_run: Tick,
    pub(crate) world: Option<UnsafeWorldCell<'w>>,
    sparse_scratch: Option<&'w mut SparseRowScratch>,
    pub(crate) msg_cursors: Option<&'w mut [Option<Box<dyn Any + Send + Sync>>]>,
    pub(crate) in_par_iter: bool,
    chunks_yielded: usize,
    entities_yielded: usize,

    current_entities_ptr: *const FfiEntity,
    current_entity_count: usize,
    /// Bitmask tracking which writable terms have already marked ticks in the current chunk.
    marked_changed_mask: u32,
    /// Bitmask tracking which sparse terms have materialized indirect row pointers in the current chunk.
    materialized_sparse_mask: u32,
    /// Bitmask tracking which sparse terms are actually present on the current archetype.
    present_sparse_mask: u32,
    active_columns: [Option<ActiveDenseColumn<'w>>; MAX_TERMS_PER_QUERY],
    /// Synthesized contiguous `[FfiBool::TRUE; run_len]` scratch buffer for present `Has<T>` terms.
    has_true_buf: Vec<FfiBool>,
    /// Synthesized contiguous `[FfiBool::FALSE; run_len]` scratch buffer for absent `Has<T>` terms.
    has_false_buf: Vec<FfiBool>,
}

const _: () = assert!(core::mem::offset_of!(HostIter<'_>, header) == 0);

impl<'w> HostIter<'w> {
    /// Constructs a dense `Table` chunk iterator.
    pub fn new_dense(
        tables: &'w Tables,
        matched_tables: &'w [TableId],
        plan: &'w QueryPlan,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        let backing = if plan.has_entity_terms {
            IterBacking::DenseTables {
                matched_tables,
                table_cursor: 0,
                row_cursor: 0,
                single_table_end_row: None,
            }
        } else {
            IterBacking::ResourceOnly
        };
        Self {
            header: HandleHeader::new(ITER_MAGIC),
            tables: Some(tables),
            backing,
            plan,
            last_run,
            this_run,
            world: None,
            sparse_scratch: None,
            msg_cursors: None,
            in_par_iter: false,
            chunks_yielded: 0,
            entities_yielded: 0,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            marked_changed_mask: 0,
            materialized_sparse_mask: 0,
            present_sparse_mask: 0,
            active_columns: [None; MAX_TERMS_PER_QUERY],
            has_true_buf: Vec::new(),
            has_false_buf: Vec::new(),
        }
    }

    /// Constructs an archetype-aware iterator supporting indirect `SparseSet` row batching.
    #[allow(clippy::too_many_arguments)]
    pub fn new_archetypal(
        tables: &'w Tables,
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        plan: &'w QueryPlan,
        sparse_scratch: &'w mut SparseRowScratch,
        last_run: Tick,
        this_run: Tick,
    ) -> Self {
        sparse_scratch.ensure_terms(plan.terms.len());
        let backing = if plan.has_entity_terms {
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                archetype_cursor: 0,
                entity_cursor: 0,
                single_arch_end_entity: None,
            }
        } else {
            IterBacking::ResourceOnly
        };
        Self {
            header: HandleHeader::new(ITER_MAGIC),
            tables: Some(tables),
            backing,
            plan,
            last_run,
            this_run,
            world: None,
            sparse_scratch: Some(sparse_scratch),
            msg_cursors: None,
            in_par_iter: false,
            chunks_yielded: 0,
            entities_yielded: 0,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            marked_changed_mask: 0,
            materialized_sparse_mask: 0,
            present_sparse_mask: 0,
            active_columns: [None; MAX_TERMS_PER_QUERY],
            has_true_buf: Vec::new(),
            has_false_buf: Vec::new(),
        }
    }

    /// Overrides the handle generation (for zero-atomic per-frame initialization when `generation != 0`).
    #[inline]
    pub fn with_generation(mut self, generation: u32) -> Self {
        self.header = HandleHeader::with_generation(ITER_MAGIC, generation);
        self
    }

    /// Attaches `UnsafeWorldCell` so `res_ptr` and `res_mut_ptr` can resolve singleton resources.
    #[inline]
    pub fn with_world(mut self, world: UnsafeWorldCell<'w>) -> Self {
        self.world = Some(world);
        self
    }

    /// Attaches per-system `MessageCursor` state for `MESSAGE_READ` terms.
    #[inline]
    pub fn with_msg_cursors(
        mut self,
        cursors: &'w mut [Option<Box<dyn Any + Send + Sync>>],
    ) -> Self {
        self.msg_cursors = Some(cursors);
        self
    }

    /// Returns `(chunks_yielded, entities_yielded)` accumulated during this iterator's lifetime.
    #[inline]
    pub const fn stats(&self) -> (usize, usize) {
        (self.chunks_yielded, self.entities_yielded)
    }

    /// Casts `&mut self` to an opaque `*mut BamIter` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamIter {
        (self as *mut Self).cast::<BamIter>()
    }

    #[inline(always)]
    fn row_matches_dense_slices(
        slices: &[&[UnsafeCell<Tick>]],
        row: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        let mut ok = true;
        for ticks in slices {
            // SAFETY: `row < table_len == ticks.len()`, and shared tick access was declared in `FilteredAccess`.
            let tick = unsafe { *ticks.get_unchecked(row).get() };
            ok &= tick.is_newer_than(last_run, this_run);
        }
        ok
    }

    #[inline(always)]
    fn row_matches_dense_slices_and_or(
        slices: &[&[UnsafeCell<Tick>]],
        or_slices: &[&[UnsafeCell<Tick>]],
        or_ranges: &[(usize, usize)],
        row: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        if !Self::row_matches_dense_slices(slices, row, last_run, this_run) {
            return false;
        }
        for &(start, end) in or_ranges {
            let mut any_match = false;
            for ticks in &or_slices[start..end] {
                // SAFETY: `row < table_len == ticks.len()`, and shared tick access was declared in `FilteredAccess`.
                let tick = unsafe { *ticks.get_unchecked(row).get() };
                if tick.is_newer_than(last_run, this_run) {
                    any_match = true;
                    break;
                }
            }
            if !any_match {
                return false;
            }
        }
        true
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn entity_matches_archetypal_filters(
        dense_slices: &[(usize, &[UnsafeCell<Tick>])],
        sparse_sets: &'w SparseSets,
        tick_filters: &[TickFilterTerm],
        entity: Entity,
        table_row_idx: usize,
        table_len: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        if table_row_idx >= table_len {
            return false;
        }
        for &(_, ticks) in dense_slices {
            // SAFETY: `table_row_idx < table_len == ticks.len()`.
            let tick = unsafe { *ticks.get_unchecked(table_row_idx).get() };
            if !tick.is_newer_than(last_run, this_run) {
                return false;
            }
        }
        for tf in tick_filters {
            if !tf.is_sparse {
                continue;
            }
            let Some(ss) = sparse_sets.get(tf.component_id) else {
                return false;
            };
            let tick_cell = if tf.is_added {
                ss.get_added_tick(entity)
            } else {
                ss.get_changed_tick(entity)
            };
            let Some(cell) = tick_cell else {
                return false;
            };
            // SAFETY: Shared sparse component tick access was declared in `FilteredAccess`.
            let tick = unsafe { *cell.get() };
            if !tick.is_newer_than(last_run, this_run) {
                return false;
            }
        }
        true
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn entity_matches_archetypal_or_groups(
        or_dense_slices: &[&[UnsafeCell<Tick>]],
        or_sparse_clauses: &[(ComponentId, bool)],
        or_ranges: &[(usize, usize, usize, usize)],
        sparse_sets: &'w SparseSets,
        entity: Entity,
        table_row_idx: usize,
        table_len: usize,
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        if table_row_idx >= table_len {
            return false;
        }
        for &(d_start, d_end, s_start, s_end) in or_ranges {
            let mut any_match = false;
            for ticks in &or_dense_slices[d_start..d_end] {
                // SAFETY: `table_row_idx < table_len == ticks.len()`.
                let tick = unsafe { *ticks.get_unchecked(table_row_idx).get() };
                if tick.is_newer_than(last_run, this_run) {
                    any_match = true;
                    break;
                }
            }
            if !any_match {
                for &(cid, is_added) in &or_sparse_clauses[s_start..s_end] {
                    let Some(ss) = sparse_sets.get(cid) else {
                        continue;
                    };
                    let tick_cell = if is_added {
                        ss.get_added_tick(entity)
                    } else {
                        ss.get_changed_tick(entity)
                    };
                    let Some(cell) = tick_cell else {
                        continue;
                    };
                    // SAFETY: Shared sparse component tick access was declared in `FilteredAccess`.
                    let tick = unsafe { *cell.get() };
                    if tick.is_newer_than(last_run, this_run) {
                        any_match = true;
                        break;
                    }
                }
            }
            if !any_match {
                return false;
            }
        }
        true
    }

    #[inline]
    fn prepare_dense_table_filters<'t>(
        table: &'t bevy_ecs::storage::Table,
        full_table_len: usize,
        plan: &QueryPlan,
        last_run: Tick,
        this_run: Tick,
    ) -> DenseTableFilters<'t> {
        let tick_filters = plan.tick_filters.as_slice();
        let or_groups = plan.or_groups.as_slice();
        let mut filter_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY] =
            [&[]; MAX_TERMS_PER_QUERY];
        let mut skip = false;

        for (f_idx, tf) in tick_filters.iter().enumerate() {
            let Some(col) = table.get_column(tf.component_id) else {
                skip = true;
                break;
            };
            if !tf.is_added
                && let Some(summary) = col.get_summary_tick()
                && !summary.get().is_newer_than(last_run, this_run)
            {
                skip = true;
                break;
            }
            filter_slices[f_idx] = if tf.is_added {
                // SAFETY: `full_table_len == table.entity_count() as usize`.
                unsafe { col.get_added_ticks_slice(full_table_len) }
            } else {
                // SAFETY: `full_table_len == table.entity_count() as usize`.
                unsafe { col.get_changed_ticks_slice(full_table_len) }
            };
        }

        let mut or_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY] =
            [&[]; MAX_TERMS_PER_QUERY];
        let mut or_slice_len = 0usize;
        let mut or_ranges: [(usize, usize); MAX_TERMS_PER_QUERY] = [(0, 0); MAX_TERMS_PER_QUERY];
        let mut or_group_len = 0usize;

        if !skip && !or_groups.is_empty() {
            for group in or_groups {
                let mut trivially_satisfied = false;
                let start_slice = or_slice_len;
                for clause in &group.terms {
                    match clause.mode {
                        AccessMode::FILTER_ONLY => {
                            if table.has_column(clause.component_id) {
                                trivially_satisfied = true;
                                break;
                            }
                        }
                        AccessMode::WITHOUT => {
                            if !table.has_column(clause.component_id) {
                                trivially_satisfied = true;
                                break;
                            }
                        }
                        AccessMode::CHANGED | AccessMode::ADDED => {
                            let Some(col) = table.get_column(clause.component_id) else {
                                continue;
                            };
                            let is_added = clause.mode == AccessMode::ADDED;
                            if !is_added
                                && let Some(summary) = col.get_summary_tick()
                                && !summary.get().is_newer_than(last_run, this_run)
                            {
                                continue;
                            }
                            let slice = if is_added {
                                // SAFETY: `full_table_len == table.entity_count() as usize`.
                                unsafe { col.get_added_ticks_slice(full_table_len) }
                            } else {
                                // SAFETY: `full_table_len == table.entity_count() as usize`.
                                unsafe { col.get_changed_ticks_slice(full_table_len) }
                            };
                            or_slices[or_slice_len] = slice;
                            or_slice_len += 1;
                        }
                        _ => {}
                    }
                }
                if trivially_satisfied {
                    or_slice_len = start_slice;
                    continue;
                }
                if or_slice_len == start_slice {
                    skip = true;
                    break;
                }
                or_ranges[or_group_len] = (start_slice, or_slice_len);
                or_group_len += 1;
            }
        }

        DenseTableFilters {
            skip,
            filter_slices,
            filter_len: tick_filters.len(),
            or_slices,
            or_slice_len,
            or_ranges,
            or_group_len,
        }
    }

    #[inline]
    fn prepare_archetype_filters<'t>(
        archetype: &bevy_ecs::archetype::Archetype,
        table: &'t bevy_ecs::storage::Table,
        sparse_sets: &SparseSets,
        table_len: usize,
        plan: &QueryPlan,
        last_run: Tick,
        this_run: Tick,
    ) -> ArchetypeFilters<'t> {
        let tick_filters = plan.tick_filters.as_slice();
        let or_groups = plan.or_groups.as_slice();
        let mut dense_filter_slices: [(usize, &'t [UnsafeCell<Tick>]); MAX_TERMS_PER_QUERY] =
            [(0, &[]); MAX_TERMS_PER_QUERY];
        let mut dense_filter_len = 0usize;
        let mut skip = false;

        for (f_idx, tf) in tick_filters.iter().enumerate() {
            if tf.is_sparse {
                continue;
            }
            let Some(col) = table.get_column(tf.component_id) else {
                skip = true;
                break;
            };
            if !tf.is_added
                && let Some(summary) = col.get_summary_tick()
                && !summary.get().is_newer_than(last_run, this_run)
            {
                skip = true;
                break;
            }
            let slice = if tf.is_added {
                // SAFETY: `table_len == table.entity_count() as usize`.
                unsafe { col.get_added_ticks_slice(table_len) }
            } else {
                // SAFETY: `table_len == table.entity_count() as usize`.
                unsafe { col.get_changed_ticks_slice(table_len) }
            };
            dense_filter_slices[dense_filter_len] = (f_idx, slice);
            dense_filter_len += 1;
        }

        let mut or_dense_slices: [&'t [UnsafeCell<Tick>]; MAX_TERMS_PER_QUERY] =
            [&[]; MAX_TERMS_PER_QUERY];
        let mut or_dense_len = 0usize;
        let mut or_sparse_clauses: [(ComponentId, bool); MAX_TERMS_PER_QUERY] =
            [(ComponentId::new(0), false); MAX_TERMS_PER_QUERY];
        let mut or_sparse_len = 0usize;
        let mut or_ranges: [(usize, usize, usize, usize); MAX_TERMS_PER_QUERY] =
            [(0, 0, 0, 0); MAX_TERMS_PER_QUERY];
        let mut or_group_len = 0usize;

        if !skip && !or_groups.is_empty() {
            for group in or_groups {
                let mut trivially_satisfied = false;
                let d_start = or_dense_len;
                let s_start = or_sparse_len;
                for clause in &group.terms {
                    match clause.mode {
                        AccessMode::FILTER_ONLY => {
                            if archetype.contains(clause.component_id) {
                                trivially_satisfied = true;
                                break;
                            }
                        }
                        AccessMode::WITHOUT => {
                            if !archetype.contains(clause.component_id) {
                                trivially_satisfied = true;
                                break;
                            }
                        }
                        AccessMode::CHANGED | AccessMode::ADDED => {
                            if !archetype.contains(clause.component_id) {
                                continue;
                            }
                            let is_added = clause.mode == AccessMode::ADDED;
                            if clause.is_sparse {
                                if sparse_sets.get(clause.component_id).is_some() {
                                    or_sparse_clauses[or_sparse_len] =
                                        (clause.component_id, is_added);
                                    or_sparse_len += 1;
                                }
                            } else if let Some(col) = table.get_column(clause.component_id) {
                                if !is_added
                                    && let Some(summary) = col.get_summary_tick()
                                    && !summary.get().is_newer_than(last_run, this_run)
                                {
                                    continue;
                                }
                                let slice = if is_added {
                                    // SAFETY: `table_len == table.entity_count() as usize`.
                                    unsafe { col.get_added_ticks_slice(table_len) }
                                } else {
                                    // SAFETY: `table_len == table.entity_count() as usize`.
                                    unsafe { col.get_changed_ticks_slice(table_len) }
                                };
                                or_dense_slices[or_dense_len] = slice;
                                or_dense_len += 1;
                            }
                        }
                        _ => {}
                    }
                }
                if trivially_satisfied {
                    or_dense_len = d_start;
                    or_sparse_len = s_start;
                    continue;
                }
                if or_dense_len == d_start && or_sparse_len == s_start {
                    skip = true;
                    break;
                }
                or_ranges[or_group_len] = (d_start, or_dense_len, s_start, or_sparse_len);
                or_group_len += 1;
            }
        }

        ArchetypeFilters {
            skip,
            dense_filter_slices,
            dense_filter_len,
            or_dense_slices,
            or_dense_len,
            or_sparse_clauses,
            or_sparse_len,
            or_ranges,
            or_group_len,
        }
    }

    /// Counts matching entities across this iterator without advancing or mutating cursor state.
    pub(crate) fn count_matching(&self) -> usize {
        let Some(tables) = self.tables else {
            return 0;
        };
        let plan = self.plan;
        let last_run = self.last_run;
        let this_run = self.this_run;
        let has_tick_filters = !plan.tick_filters.is_empty();
        let has_or_groups = !plan.or_groups.is_empty();

        match self.backing {
            IterBacking::ResourceOnly => 0,
            IterBacking::DenseTables {
                matched_tables,
                table_cursor,
                row_cursor,
                single_table_end_row,
            } => {
                let mut total = 0usize;
                for (t_idx, &table_id) in matched_tables.iter().enumerate() {
                    let Some(table) = tables.get(table_id) else {
                        continue;
                    };
                    let full_len = table.entity_count() as usize;
                    let limit_len = single_table_end_row
                        .map(|end| end.min(full_len))
                        .unwrap_or(full_len);
                    let start = if self.in_par_iter
                        && single_table_end_row.is_some()
                        && t_idx == table_cursor
                    {
                        row_cursor.min(limit_len)
                    } else {
                        0
                    };
                    if start >= limit_len {
                        continue;
                    }
                    if !has_tick_filters && !has_or_groups {
                        total += limit_len - start;
                        continue;
                    }
                    let df = Self::prepare_dense_table_filters(
                        table, full_len, plan, last_run, this_run,
                    );
                    if df.skip {
                        continue;
                    }
                    let active_slices = &df.filter_slices[..df.filter_len];
                    let active_or_slices = &df.or_slices[..df.or_slice_len];
                    let active_or_ranges = &df.or_ranges[..df.or_group_len];
                    for row in start..limit_len {
                        if Self::row_matches_dense_slices_and_or(
                            active_slices,
                            active_or_slices,
                            active_or_ranges,
                            row,
                            last_run,
                            this_run,
                        ) {
                            total += 1;
                        }
                    }
                }
                total
            }
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                archetype_cursor,
                entity_cursor,
                single_arch_end_entity,
            } => {
                let tick_filters = plan.tick_filters.as_slice();
                let mut total = 0usize;
                for (a_idx, &arch_id) in matched_archetypes.iter().enumerate() {
                    let Some(archetype) = archetypes.get(arch_id) else {
                        continue;
                    };
                    let arch_entities = archetype.entities();
                    let limit_entities = single_arch_end_entity
                        .map(|end| end.min(arch_entities.len()))
                        .unwrap_or(arch_entities.len());
                    let start = if self.in_par_iter
                        && single_arch_end_entity.is_some()
                        && a_idx == archetype_cursor
                    {
                        entity_cursor.min(limit_entities)
                    } else {
                        0
                    };
                    if start >= limit_entities {
                        continue;
                    }
                    if !has_tick_filters && !has_or_groups {
                        total += limit_entities - start;
                        continue;
                    }
                    let Some(table) = tables.get(archetype.table_id()) else {
                        continue;
                    };
                    let table_len = table.entity_count() as usize;
                    let af = Self::prepare_archetype_filters(
                        archetype,
                        table,
                        sparse_sets,
                        table_len,
                        plan,
                        last_run,
                        this_run,
                    );
                    if af.skip {
                        continue;
                    }
                    let active_dense = &af.dense_filter_slices[..af.dense_filter_len];
                    let active_or_dense = &af.or_dense_slices[..af.or_dense_len];
                    let active_or_sparse = &af.or_sparse_clauses[..af.or_sparse_len];
                    let active_or_ranges = &af.or_ranges[..af.or_group_len];
                    for cand in &arch_entities[start..limit_entities] {
                        let row_idx = cand.table_row().index();
                        let ok_top = !has_tick_filters
                            || Self::entity_matches_archetypal_filters(
                                active_dense,
                                sparse_sets,
                                tick_filters,
                                cand.id(),
                                row_idx,
                                table_len,
                                last_run,
                                this_run,
                            );
                        let ok_or = active_or_ranges.is_empty()
                            || Self::entity_matches_archetypal_or_groups(
                                active_or_dense,
                                active_or_sparse,
                                active_or_ranges,
                                sparse_sets,
                                cand.id(),
                                row_idx,
                                table_len,
                                last_run,
                                this_run,
                            );
                        if ok_top && ok_or {
                            total += 1;
                        }
                    }
                }
                total
            }
        }
    }

    /// Returns `true` if the query plan declares any mutable (`WRITE` / `OPTIONAL_WRITE`) term.
    #[inline]
    pub(crate) fn has_writable_terms(&self) -> bool {
        self.plan
            .terms
            .iter()
            .any(|t| matches!(t.mode, AccessMode::WRITE | AccessMode::OPTIONAL_WRITE))
    }

    /// Advances to the next non-empty matching chunk, returning `0` when exhausted.
    pub fn next_chunk(&mut self) -> usize {
        let last_run = self.last_run;
        let this_run = self.this_run;
        let plan = self.plan;
        let Some(tables) = self.tables else {
            self.clear_active_chunk();
            return 0;
        };

        match self.backing {
            IterBacking::ResourceOnly => {
                self.clear_active_chunk();
                return 0;
            }
            IterBacking::DenseTables {
                matched_tables,
                ref mut table_cursor,
                ref mut row_cursor,
                single_table_end_row,
            } => {
                let tick_filters = plan.tick_filters.as_slice();
                let or_groups = plan.or_groups.as_slice();
                let has_tick_filters = !tick_filters.is_empty();
                let has_or_groups = !or_groups.is_empty();

                while *table_cursor < matched_tables.len() {
                    let table_id = matched_tables[*table_cursor];
                    let Some(table) = tables.get(table_id) else {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    };

                    let full_table_len = table.entity_count() as usize;
                    let limit_len = single_table_end_row
                        .map(|end| end.min(full_table_len))
                        .unwrap_or(full_table_len);
                    if limit_len == 0 || *row_cursor >= limit_len {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    if !has_tick_filters && !has_or_groups {
                        let start = *row_cursor;
                        let run_len = limit_len - start;
                        *table_cursor += 1;
                        *row_cursor = 0;
                        // SAFETY: `start + run_len == limit_len <= full_table_len == table.entities().len()`.
                        self.current_entities_ptr =
                            unsafe { table.entities().as_ptr().add(start).cast::<FfiEntity>() };
                        self.current_entity_count = run_len;
                        self.populate_dense_columns(table, full_table_len, start, run_len);
                        self.populate_has_columns(run_len, |cid| table.has_column(cid));
                        self.chunks_yielded += 1;
                        self.entities_yielded += run_len;
                        return run_len;
                    }

                    let df = Self::prepare_dense_table_filters(
                        table,
                        full_table_len,
                        plan,
                        last_run,
                        this_run,
                    );
                    if df.skip {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    let active_slices = &df.filter_slices[..df.filter_len];
                    let active_or_slices = &df.or_slices[..df.or_slice_len];
                    let active_or_ranges = &df.or_ranges[..df.or_group_len];

                    let (start_row_idx, run_len) =
                        if active_slices.is_empty() && active_or_ranges.is_empty() {
                            let start = *row_cursor;
                            (start, limit_len - start)
                        } else {
                            let mut start_row_idx = *row_cursor;
                            while start_row_idx < limit_len
                                && !Self::row_matches_dense_slices_and_or(
                                    active_slices,
                                    active_or_slices,
                                    active_or_ranges,
                                    start_row_idx,
                                    last_run,
                                    this_run,
                                )
                            {
                                start_row_idx += 1;
                            }

                            if start_row_idx >= limit_len {
                                *table_cursor += 1;
                                *row_cursor = 0;
                                continue;
                            }

                            let mut run_len = 1usize;
                            while start_row_idx + run_len < limit_len
                                && Self::row_matches_dense_slices_and_or(
                                    active_slices,
                                    active_or_slices,
                                    active_or_ranges,
                                    start_row_idx + run_len,
                                    last_run,
                                    this_run,
                                )
                            {
                                run_len += 1;
                            }
                            (start_row_idx, run_len)
                        };

                    *row_cursor = start_row_idx + run_len;
                    if *row_cursor >= limit_len {
                        *table_cursor += 1;
                        *row_cursor = 0;
                    }

                    // SAFETY: `start_row_idx + run_len <= limit_len <= full_table_len == table.entities().len()`.
                    self.current_entities_ptr = unsafe {
                        table
                            .entities()
                            .as_ptr()
                            .add(start_row_idx)
                            .cast::<FfiEntity>()
                    };
                    self.current_entity_count = run_len;
                    self.populate_dense_columns(table, full_table_len, start_row_idx, run_len);
                    self.populate_has_columns(run_len, |cid| table.has_column(cid));
                    self.chunks_yielded += 1;
                    self.entities_yielded += run_len;
                    return run_len;
                }
            }
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                ref mut archetype_cursor,
                ref mut entity_cursor,
                single_arch_end_entity,
            } => {
                let tick_filters = plan.tick_filters.as_slice();
                let has_tick_filters = !tick_filters.is_empty();
                let require_contiguous_table_rows = plan.dense_data_mask != 0;

                while *archetype_cursor < matched_archetypes.len() {
                    let arch_id = matched_archetypes[*archetype_cursor];
                    let Some(archetype) = archetypes.get(arch_id) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };

                    let arch_entities = archetype.entities();
                    let limit_entities = single_arch_end_entity
                        .map(|end| end.min(arch_entities.len()))
                        .unwrap_or(arch_entities.len());
                    if *entity_cursor >= limit_entities {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    }

                    let Some(table) = tables.get(archetype.table_id()) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };
                    let table_len = table.entity_count() as usize;

                    let af = Self::prepare_archetype_filters(
                        archetype,
                        table,
                        sparse_sets,
                        table_len,
                        plan,
                        last_run,
                        this_run,
                    );
                    if af.skip {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    }

                    let active_dense_filters = &af.dense_filter_slices[..af.dense_filter_len];
                    let active_or_dense = &af.or_dense_slices[..af.or_dense_len];
                    let active_or_sparse = &af.or_sparse_clauses[..af.or_sparse_len];
                    let active_or_ranges = &af.or_ranges[..af.or_group_len];
                    let evaluate_row_filters = has_tick_filters || !active_or_ranges.is_empty();

                    if evaluate_row_filters {
                        while *entity_cursor < limit_entities {
                            let cand = &arch_entities[*entity_cursor];
                            let row_idx = cand.table_row().index();
                            let ok_top = !has_tick_filters
                                || Self::entity_matches_archetypal_filters(
                                    active_dense_filters,
                                    sparse_sets,
                                    tick_filters,
                                    cand.id(),
                                    row_idx,
                                    table_len,
                                    last_run,
                                    this_run,
                                );
                            let ok_or = active_or_ranges.is_empty()
                                || Self::entity_matches_archetypal_or_groups(
                                    active_or_dense,
                                    active_or_sparse,
                                    active_or_ranges,
                                    sparse_sets,
                                    cand.id(),
                                    row_idx,
                                    table_len,
                                    last_run,
                                    this_run,
                                );
                            if ok_top && ok_or {
                                break;
                            }
                            *entity_cursor += 1;
                        }
                        if *entity_cursor >= limit_entities {
                            *archetype_cursor += 1;
                            *entity_cursor = 0;
                            continue;
                        }
                    }

                    let start_entity_idx = *entity_cursor;
                    let first = &arch_entities[start_entity_idx];
                    let start_row_idx = first.table_row().index();
                    let mut contiguous_table_rows = true;

                    // Batch multi-entity runs even when `sparse_data_mask != 0`!
                    let mut run_len = 1usize;
                    while start_entity_idx + run_len < limit_entities {
                        let next_ent = &arch_entities[start_entity_idx + run_len];
                        let next_row_idx = next_ent.table_row().index();
                        if next_row_idx != start_row_idx + run_len {
                            if require_contiguous_table_rows {
                                break;
                            }
                            contiguous_table_rows = false;
                        }
                        if evaluate_row_filters {
                            let ok_top = !has_tick_filters
                                || Self::entity_matches_archetypal_filters(
                                    active_dense_filters,
                                    sparse_sets,
                                    tick_filters,
                                    next_ent.id(),
                                    next_row_idx,
                                    table_len,
                                    last_run,
                                    this_run,
                                );
                            let ok_or = active_or_ranges.is_empty()
                                || Self::entity_matches_archetypal_or_groups(
                                    active_or_dense,
                                    active_or_sparse,
                                    active_or_ranges,
                                    sparse_sets,
                                    next_ent.id(),
                                    next_row_idx,
                                    table_len,
                                    last_run,
                                    this_run,
                                );
                            if !(ok_top && ok_or) {
                                break;
                            }
                        }
                        run_len += 1;
                    }

                    *entity_cursor = start_entity_idx + run_len;
                    if *entity_cursor >= limit_entities {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                    }

                    self.clear_active_chunk();
                    self.current_entity_count = run_len;
                    self.chunks_yielded += 1;
                    self.entities_yielded += run_len;

                    if contiguous_table_rows && start_row_idx + run_len <= table_len {
                        // SAFETY: `start_row_idx + run_len <= table_len == table.entities().len()`.
                        self.current_entities_ptr = unsafe {
                            table
                                .entities()
                                .as_ptr()
                                .add(start_row_idx)
                                .cast::<FfiEntity>()
                        };
                    } else if let Some(ref mut scratch) = self.sparse_scratch {
                        scratch.entities.clear();
                        scratch.entities.reserve(run_len);
                        for arch_ent in &arch_entities[start_entity_idx..start_entity_idx + run_len]
                        {
                            scratch
                                .entities
                                .push(FfiEntity::from_bits(arch_ent.id().to_bits()));
                        }
                        self.current_entities_ptr = scratch.entities.as_ptr();
                    }

                    if plan.dense_data_mask != 0 {
                        self.populate_dense_columns_inner(table, table_len, start_row_idx, run_len);
                    }
                    self.populate_has_columns(run_len, |cid| archetype.contains(cid));

                    // Eagerly materialize sparse row pointers for declared sparse data columns into
                    // reused scratch buffers so `iter_column_rows_readonly`/`mutable` are O(1).
                    if plan.sparse_data_mask != 0
                        && let Some(ref mut scratch) = self.sparse_scratch
                    {
                        let mut mask = plan.sparse_data_mask;
                        while mask != 0 {
                            let idx = mask.trailing_zeros() as usize;
                            mask &= mask - 1;
                            let term = &plan.terms[idx];
                            if !archetype.contains(term.component_id) {
                                continue;
                            }
                            let Some(ss) = sparse_sets.get(term.component_id) else {
                                continue;
                            };
                            let rows_vec = &mut scratch.row_ptrs[idx];
                            let ticks_vec = &mut scratch.tick_ptrs[idx];
                            rows_vec.clear();
                            ticks_vec.clear();
                            rows_vec.reserve(run_len);
                            ticks_vec.reserve(run_len);
                            let mut summary_ptr: Option<*const AtomicTick> = None;
                            let mut all_present = true;

                            for arch_ent in
                                &arch_entities[start_entity_idx..start_entity_idx + run_len]
                            {
                                if let Some((ptr, tick_cells)) = ss.get_with_ticks(arch_ent.id()) {
                                    rows_vec.push(ptr.as_ptr().cast_mut());
                                    ticks_vec.push(tick_cells.changed as *const UnsafeCell<Tick>);
                                    if summary_ptr.is_none()
                                        && let Some(s) = tick_cells.summary_tick
                                    {
                                        summary_ptr = Some(s as *const AtomicTick);
                                    }
                                } else {
                                    all_present = false;
                                    break;
                                }
                            }

                            if all_present {
                                scratch.summary_ptrs[idx] = summary_ptr;
                                self.present_sparse_mask |= 1u32 << idx;
                                self.materialized_sparse_mask |= 1u32 << idx;
                            }
                        }
                    }

                    return run_len;
                }
            }
        }

        self.clear_active_chunk();
        0
    }

    /// Resets this iterator back to the first matched chunk (`iter_reset`).
    #[inline]
    pub fn reset(&mut self) {
        match &mut self.backing {
            IterBacking::ResourceOnly => {}
            IterBacking::DenseTables {
                table_cursor,
                row_cursor,
                ..
            } => {
                *table_cursor = 0;
                *row_cursor = 0;
            }
            IterBacking::Archetypes {
                archetype_cursor,
                entity_cursor,
                ..
            } => {
                *archetype_cursor = 0;
                *entity_cursor = 0;
            }
        }
        self.clear_active_chunk();
    }

    /// Clears only the active prefix `0..term_count` and resets per-chunk bitmasks.
    #[inline]
    fn clear_active_chunk(&mut self) {
        self.current_entities_ptr = core::ptr::null();
        self.current_entity_count = 0;
        self.marked_changed_mask = 0;
        self.materialized_sparse_mask = 0;
        self.present_sparse_mask = 0;
        let active_len = self.plan.terms.len().min(MAX_TERMS_PER_QUERY);
        for slot in &mut self.active_columns[..active_len] {
            *slot = None;
        }
    }

    #[inline]
    fn populate_dense_columns(
        &mut self,
        table: &'w Table,
        table_len: usize,
        start_row_idx: usize,
        run_len: usize,
    ) {
        self.marked_changed_mask = 0;
        self.materialized_sparse_mask = 0;
        self.present_sparse_mask = 0;
        let active_len = self.plan.terms.len().min(MAX_TERMS_PER_QUERY);
        for slot in &mut self.active_columns[..active_len] {
            *slot = None;
        }
        self.populate_dense_columns_inner(table, table_len, start_row_idx, run_len);
    }

    #[inline]
    fn populate_dense_columns_inner(
        &mut self,
        table: &'w Table,
        table_len: usize,
        start_row_idx: usize,
        run_len: usize,
    ) {
        let Some(start_nonmax) = NonMaxU32::new(start_row_idx as u32) else {
            return;
        };
        let start_row = TableRow::new(start_nonmax);

        // Iterate ONLY precomputed dense data-column indices!
        for &raw_idx in &self.plan.dense_data_indices {
            let idx = raw_idx as usize;
            let term = &self.plan.terms[idx];
            if let Some(col) = table.get_column(term.component_id) {
                // SAFETY: `start_row_idx + run_len <= table_len == table.entity_count() as usize`.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr().cast_mut() };
                // SAFETY: `table_len` is `table.entity_count() as usize`.
                let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                let changed_ticks = &all_ticks[start_row_idx..start_row_idx + run_len];
                let summary_tick = col.get_summary_tick();

                self.active_columns[idx] = Some(ActiveDenseColumn {
                    data_ptr,
                    changed_ticks,
                    summary_tick,
                });
            }
        }
    }

    #[inline]
    fn populate_has_columns(
        &mut self,
        run_len: usize,
        mut is_present: impl FnMut(ComponentId) -> bool,
    ) {
        if self.plan.has_indices.is_empty() || run_len == 0 {
            return;
        }
        let mut presence_mask = 0u32;
        let mut need_true = false;
        let mut need_false = false;
        for &raw_idx in &self.plan.has_indices {
            let idx = raw_idx as usize;
            let term = &self.plan.terms[idx];
            if is_present(term.component_id) {
                presence_mask |= 1u32 << idx;
                need_true = true;
            } else {
                need_false = true;
            }
        }
        if need_true && self.has_true_buf.len() < run_len {
            self.has_true_buf.resize(run_len, FfiBool::TRUE);
        }
        if need_false && self.has_false_buf.len() < run_len {
            self.has_false_buf.resize(run_len, FfiBool::FALSE);
        }
        let true_ptr = if need_true {
            self.has_true_buf.as_ptr().cast_mut().cast::<u8>()
        } else {
            core::ptr::null_mut()
        };
        let false_ptr = if need_false {
            self.has_false_buf.as_ptr().cast_mut().cast::<u8>()
        } else {
            core::ptr::null_mut()
        };
        for &raw_idx in &self.plan.has_indices {
            let idx = raw_idx as usize;
            let data_ptr = if (presence_mask & (1u32 << idx)) != 0 {
                true_ptr
            } else {
                false_ptr
            };
            self.active_columns[idx] = Some(ActiveDenseColumn {
                data_ptr,
                changed_ticks: &[],
                summary_tick: None,
            });
        }
    }
}

impl Drop for HostIter<'_> {
    #[inline]
    fn drop(&mut self) {
        self.header.poison();
    }
}

/// Advances `iter` to the next non-empty chunk, writing chunk length into `*out_len` and returning
/// [`FfiBool::TRUE`] while chunks remain or [`FfiBool::FALSE`] when exhausted.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_len` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_iter_next_chunk(iter: *mut BamIter, out_len: *mut usize) -> FfiBool {
    if out_len.is_null() {
        return FfiBool::FALSE;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return FfiBool::FALSE;
    };

    let len = host_iter.next_chunk();
    // SAFETY: `out_len` was checked non-null above and caller guarantees writable `usize`.
    unsafe {
        *out_len = len;
    }
    FfiBool::from(len > 0)
}

/// Resets `iter` back to its initial chunk (`FfiResult::OK`).
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_reset(iter: *mut BamIter) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    host_iter.reset();
    FfiResult::OK
}

/// Returns a pointer to `[FfiEntity; chunk_len]` for the current chunk, or null.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_entities(iter: *const BamIter) -> *const FfiEntity {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 {
        return core::ptr::null();
    }
    host_iter.current_entities_ptr
}

/// Returns a read-only byte pointer to dense `Table` column `term_index` in the current chunk,
/// or null if out of bounds, absent on an optional term, sparse-set stored, or filter/resource.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_readonly(
    iter: *const BamIter,
    term_index: usize,
) -> *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if term.is_resource() || term.is_message() {
        return core::ptr::null();
    }
    if (host_iter.plan.or_term_mask & (1u32 << term_index)) != 0 {
        return core::ptr::null();
    }
    if term.mode.is_has() {
        return match &host_iter.active_columns[term_index] {
            Some(col) => col.data_ptr.cast_const(),
            None => core::ptr::null(),
        };
    }
    if !term.mode.has_entity_column_data() || term.is_sparse_set() {
        return core::ptr::null();
    }
    match &host_iter.active_columns[term_index] {
        Some(col) => col.data_ptr.cast_const(),
        None => core::ptr::null(),
    }
}

/// Returns a mutable byte pointer to dense `Table` column `term_index`, marking chunk change ticks
/// at most once per chunk.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_mutable(
    iter: *mut BamIter,
    term_index: usize,
) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null_mut();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_sparse_set()
        || term.is_immutable()
    {
        return core::ptr::null_mut();
    }

    let Some(col) = host_iter.active_columns[term_index] else {
        return core::ptr::null_mut();
    };

    let bit = 1u32 << term_index;
    if (host_iter.marked_changed_mask & bit) == 0 {
        let this_run = host_iter.this_run;
        for tick_cell in col.changed_ticks {
            // SAFETY: Exclusive mutable access to this component's rows is held by this system.
            unsafe {
                *tick_cell.get() = this_run;
            }
        }
        if let Some(summary) = col.summary_tick {
            summary.set(this_run);
        }
        host_iter.marked_changed_mask |= bit;
    }

    col.data_ptr
}

/// Returns a mutable byte pointer to dense `Table` column `term_index` without marking change ticks.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_mutable_untracked(
    iter: *mut BamIter,
    term_index: usize,
) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null_mut();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_sparse_set()
        || term.is_immutable()
    {
        return core::ptr::null_mut();
    }
    match host_iter.active_columns[term_index] {
        Some(col) => col.data_ptr,
        None => core::ptr::null_mut(),
    }
}

/// Marks rows `start_row..start_row + row_count` in writable column `term_index` as changed.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_mark_changed_range(
    iter: *mut BamIter,
    term_index: usize,
    start_row: usize,
    row_count: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };

    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return FfiResult::PERMISSION_DENIED;
    }

    let Some(end_row) = start_row.checked_add(row_count) else {
        return FfiResult::OUT_OF_RANGE;
    };
    if end_row > host_iter.current_entity_count {
        return FfiResult::OUT_OF_RANGE;
    }
    if row_count == 0 {
        return FfiResult::OK;
    }

    let this_run = host_iter.this_run;
    if term.is_sparse_set() {
        if (host_iter.present_sparse_mask & (1u32 << term_index)) == 0 {
            return if term.mode == AccessMode::OPTIONAL_WRITE {
                FfiResult::OK
            } else {
                FfiResult::INVALID_COMPONENT
            };
        }
        if let Some(ref scratch) = host_iter.sparse_scratch {
            for &tick_cell_ptr in &scratch.tick_ptrs[term_index][start_row..end_row] {
                // SAFETY: `tick_cell_ptr` points to a live `UnsafeCell<Tick>` for this chunk's entity
                // under exclusive `WRITE` / `OPTIONAL_WRITE` component access.
                unsafe {
                    *(*tick_cell_ptr).get() = this_run;
                }
            }
            if let Some(summary_ptr) = scratch.summary_ptrs[term_index] {
                // SAFETY: `summary_ptr` points to the live sparse set summary `AtomicTick`.
                unsafe {
                    (*summary_ptr).set(this_run);
                }
            }
            return FfiResult::OK;
        }
        return FfiResult::INTERNAL_ERROR;
    }

    if let Some(col) = host_iter.active_columns[term_index] {
        for tick_cell in &col.changed_ticks[start_row..end_row] {
            // SAFETY: `start_row..end_row` is in bounds and `WRITE`/`OPTIONAL_WRITE` access is held.
            unsafe {
                *tick_cell.get() = this_run;
            }
        }
        if let Some(summary) = col.summary_tick {
            summary.set(this_run);
        }
        FfiResult::OK
    } else if term.mode == AccessMode::OPTIONAL_WRITE {
        FfiResult::OK
    } else {
        FfiResult::INVALID_COMPONENT
    }
}

/// Returns `[ *const u8; chunk_len ]` of indirect per-row pointers for `StorageType::SparseSet`
/// columns, or null if out of bounds, absent on an optional term, or not sparse-set stored.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_rows_readonly(
    iter: *mut BamIter,
    term_index: usize,
) -> *const *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.is_sparse_set() || !term.mode.has_entity_column_data() {
        return core::ptr::null();
    }
    let bit = 1u32 << term_index;
    if (host_iter.present_sparse_mask & bit) == 0 {
        return core::ptr::null();
    }
    let Some(ref scratch) = host_iter.sparse_scratch else {
        return core::ptr::null();
    };
    scratch.row_ptrs[term_index].as_ptr().cast::<*const u8>()
}

/// Returns `[ *mut u8; chunk_len ]` of indirect per-row mutable pointers for `StorageType::SparseSet`
/// columns, marking matched rows changed at most once per chunk.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_iter_column_rows_mutable(
    iter: *mut BamIter,
    term_index: usize,
) -> *const *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null();
    };
    if host_iter.current_entity_count == 0 || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.is_sparse_set()
        || (term.mode != AccessMode::WRITE && term.mode != AccessMode::OPTIONAL_WRITE)
        || term.is_immutable()
    {
        return core::ptr::null();
    }
    let bit = 1u32 << term_index;
    if (host_iter.present_sparse_mask & bit) == 0 {
        return core::ptr::null();
    }

    let this_run = host_iter.this_run;
    let should_mark = (host_iter.marked_changed_mask & bit) == 0;
    let Some(ref scratch) = host_iter.sparse_scratch else {
        return core::ptr::null();
    };

    if should_mark {
        for &tick_cell_ptr in &scratch.tick_ptrs[term_index] {
            // SAFETY: Exclusive mutable access to this sparse component is held by this system.
            unsafe {
                *(*tick_cell_ptr).get() = this_run;
            }
        }
        if let Some(summary_ptr) = scratch.summary_ptrs[term_index] {
            // SAFETY: `summary_ptr` points to the live sparse set summary `AtomicTick`.
            unsafe {
                (*summary_ptr).set(this_run);
            }
        }
    }

    let out_ptr = scratch.row_ptrs[term_index].as_ptr();
    if should_mark {
        host_iter.marked_changed_mask |= bit;
    }
    out_ptr
}

/// Writes [`BamColumnInfo`] metadata for `term_index` into `*out_info`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_info` must point to writable [`BamColumnInfo`] storage, or null.
pub unsafe extern "C" fn host_iter_column_info(
    iter: *const BamIter,
    term_index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult {
    if out_info.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };

    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let t = &host_iter.plan.terms[term_index];
    let info = BamColumnInfo {
        component: t.component,
        mode: t.mode,
        uuid: t.uuid,
        size: t.size,
        align: t.align,
        storage_flags: t.storage_flags,
        reserved0: 0,
    };
    // SAFETY: `out_info` was checked non-null above and caller guarantees writable `BamColumnInfo`.
    unsafe {
        *out_info = info;
    }
    FfiResult::OK
}

/// Returns a read-only pointer to the singleton `Resource` declared at `term_index`,
/// or null if unauthorized or absent in the `World`.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_res_ptr(iter: *const BamIter, term_index: usize) -> *const u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null();
    };
    if term_index >= host_iter.plan.terms.len() {
        return core::ptr::null();
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_resource() || !term.is_resource() {
        return core::ptr::null();
    }
    let Some(world) = host_iter.world else {
        return core::ptr::null();
    };
    // SAFETY: `RESOURCE_READ` or `RESOURCE_WRITE` on `term.component_id` was declared in
    // `FilteredAccessSet` during `GuestSystem::initialize`.
    match unsafe { world.get_resource_by_id(term.component_id) } {
        Some(ptr) => ptr.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Returns an exclusive mutable pointer to the singleton `Resource` declared with `RESOURCE_WRITE`
/// at `term_index`, marking its Bevy change tick, or null if unauthorized or absent.
///
/// # Safety
/// `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
pub unsafe extern "C" fn host_res_mut_ptr(iter: *mut BamIter, term_index: usize) -> *mut u8 {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return core::ptr::null_mut();
    };
    if host_iter.in_par_iter || term_index >= host_iter.plan.terms.len() {
        return core::ptr::null_mut();
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::RESOURCE_WRITE || !term.is_resource() || term.is_immutable() {
        return core::ptr::null_mut();
    }
    let Some(world) = host_iter.world else {
        return core::ptr::null_mut();
    };
    // SAFETY: `RESOURCE_WRITE` was declared in `FilteredAccessSet` during `GuestSystem::initialize`,
    // granting exclusive mutable access to `term.component_id` and its resource entity.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return core::ptr::null_mut();
    };
    let Ok(entity_cell) =
        world.get_entity_with_ticks(entity, host_iter.last_run, host_iter.this_run)
    else {
        return core::ptr::null_mut();
    };
    // SAFETY: Exclusive mutable access to this resource component is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(term.component_id) }) else {
        return core::ptr::null_mut();
    };
    mut_untyped.into_inner().as_ptr()
}

/// Reads up to `max_count` buffered POD Bevy `Messages<M>` from `MESSAGE_READ` / `MESSAGE_WRITE`
/// term `term_index` into `out_buf`, advancing this system's dedicated `MessageCursor<M>`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `max_count > 0`, `out_buf` must be writable for `max_count * size_of::<M>()` bytes.
/// - `out_count` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_msg_read_batch(
    iter: *mut BamIter,
    term_index: usize,
    out_buf: *mut u8,
    max_count: usize,
    out_count: *mut usize,
) -> FfiResult {
    if out_count.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_message() || !term.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(read_fn) = term.msg_read_batch_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    let Some(ref mut cursors) = host_iter.msg_cursors else {
        return FfiResult::INVALID_STATE;
    };
    let Some(Some(cursor_box)) = cursors.get_mut(term_index) else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_READ` or `MESSAGE_WRITE` was declared in `FilteredAccessSet`; `cursor_box`
    // was constructed by `create_msg_cursor_impl::<M>` for `term.component_id`.
    unsafe {
        read_fn(
            world,
            term.component_id,
            cursor_box.as_mut(),
            out_buf,
            max_count,
            out_count,
        )
    }
}

/// Sends a single POD Bevy `Message` into `Messages<M>` declared with `MESSAGE_WRITE` at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `data_len > 0`, `data_ptr` must be readable for `data_len` bytes.
pub unsafe extern "C" fn host_msg_send(
    iter: *mut BamIter,
    term_index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::MESSAGE_WRITE || !term.is_message() || term.is_immutable() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(send_fn) = term.msg_send_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_WRITE` was declared in `FilteredAccessSet`; caller guarantees `data_ptr`
    // readability when `data_len > 0`.
    unsafe {
        send_fn(
            world,
            host_iter.last_run,
            host_iter.this_run,
            term.component_id,
            data_ptr,
            data_len,
        )
    }
}

/// Writes `count` contiguous POD messages into `Messages<M>` declared with `MESSAGE_WRITE` at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - If `count > 0`, `in_buf` must be readable for `count * size_of::<M>()` bytes.
pub unsafe extern "C" fn host_msg_write_batch(
    iter: *mut BamIter,
    term_index: usize,
    in_buf: *const u8,
    count: usize,
) -> FfiResult {
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::MESSAGE_WRITE || !term.is_message() || term.is_immutable() {
        return FfiResult::PERMISSION_DENIED;
    }
    if count == 0 {
        return FfiResult::OK;
    }
    if term.size > 0 && in_buf.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(_) = count.checked_mul(term.size) else {
        return FfiResult::INVALID_ARGUMENT;
    };
    let Some(write_fn) = term.msg_write_batch_fn else {
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `MESSAGE_WRITE` was declared in `FilteredAccessSet` and `in_buf` bounds were checked above.
    unsafe {
        write_fn(
            world,
            host_iter.last_run,
            host_iter.this_run,
            term.component_id,
            in_buf,
            count,
        )
    }
}

/// Reads Bevy change-detection ticks (`added`, `changed`, `last_run`, `this_run`) for the
/// singleton `Resource` declared at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_ticks` must point to aligned writable [`BamChangeTicks`] storage, or null.
pub unsafe extern "C" fn host_res_ticks(
    iter: *const BamIter,
    term_index: usize,
    out_ticks: *mut BamChangeTicks,
) -> FfiResult {
    if out_ticks.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_ticks.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_ref::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if !term.mode.is_resource() || !term.is_resource() {
        return FfiResult::PERMISSION_DENIED;
    }
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    // SAFETY: `RESOURCE_READ` or `RESOURCE_WRITE` on `term.component_id` was declared in `FilteredAccessSet`.
    let Some(entity) = (unsafe { world.resource_entities() }).get(term.component_id) else {
        return FfiResult::NOT_FOUND;
    };
    let Ok(entity_cell) = world.get_entity(entity) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: Shared or exclusive access to this resource was declared in `FilteredAccessSet`.
    let Some(ticks) = (unsafe { entity_cell.get_change_ticks_by_id(term.component_id) }) else {
        return FfiResult::NOT_FOUND;
    };
    // SAFETY: `out_ticks` is non-null, aligned, and exclusively writable here.
    unsafe {
        *out_ticks = BamChangeTicks {
            added: ticks.added.get(),
            changed: ticks.changed.get(),
            last_run: host_iter.last_run.get(),
            this_run: host_iter.this_run.get(),
        };
    }
    FfiResult::OK
}

#[derive(Clone, Copy)]
struct UnsafeWorldCellSend<'w>(Option<UnsafeWorldCell<'w>>);

// SAFETY: Used strictly within `host_par_for_each_chunk` scoped worker tasks where:
// 1. Each task operates on a disjoint `(TableId, start_row, end_row)` or `(ArchetypeId, start_idx, end_idx)` range.
// 2. Mutable singleton/message operations (`res_mut_ptr`, `msg_read_batch`, `msg_send`) are rejected
//    while `in_par_iter == true`, leaving only shared read-only `res_ptr` (`&R: Sync`) on `world`.
unsafe impl Send for UnsafeWorldCellSend<'_> {}
// SAFETY: Same scoped read-only resource and disjoint component-row invariants as `Send`.
unsafe impl Sync for UnsafeWorldCellSend<'_> {}

#[derive(Clone, Copy)]
struct TablesSend<'w>(&'w Tables);
// SAFETY: Shared read-only table metadata lookup across disjoint `(TableId, start_row, end_row)` slices
// inside `ComputeTaskPool::scope`; column/tick writes occur strictly on disjoint entity rows.
unsafe impl Send for TablesSend<'_> {}
// SAFETY: Same scoped disjoint-row invariants as `Send`.
unsafe impl Sync for TablesSend<'_> {}

#[derive(Clone, Copy)]
struct ArchetypesSend<'w>(&'w Archetypes);
// SAFETY: Archetype metadata is immutable during `GuestSystem::run_unsafe` (`Deferred` commands only).
unsafe impl Send for ArchetypesSend<'_> {}
// SAFETY: Archetype metadata is immutable during `GuestSystem::run_unsafe`.
unsafe impl Sync for ArchetypesSend<'_> {}

#[derive(Clone, Copy)]
struct SparseSetsSend<'w>(&'w SparseSets);
// SAFETY: SparseSet index maps are immutable during `GuestSystem::run_unsafe`, and each worker task
// accesses disjoint `ArchetypeEntity` indices `[start_entity..end_entity]` with task-local `SparseRowScratch`.
unsafe impl Send for SparseSetsSend<'_> {}
// SAFETY: Same scoped disjoint-entity invariants as `Send`.
unsafe impl Sync for SparseSetsSend<'_> {}

#[derive(Clone, Copy)]
struct DenseParPartition {
    table_id: TableId,
    start_row: usize,
    end_row: usize,
}

#[derive(Clone, Copy)]
struct ArchetypeParPartition {
    arch_id: ArchetypeId,
    start_entity: usize,
    end_entity: usize,
}

/// Executes `chunk_fn(task_iter, user_data)` in parallel across Bevy's [`ComputeTaskPool`] over
/// disjoint matched table/archetype sub-ranges.
///
/// Each worker task receives a scoped `BamIter` already positioned on an active non-empty chunk.
/// Non-reentrant (`in_par_iter == true` rejects nested `par_for_each_chunk` with `ACCESS_CONFLICT`),
/// and any non-`OK` `FfiResult` returned by a task (including `PANIC_CAUGHT`) aborts remaining
/// chunk dispatches and is returned deterministically to the caller.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   on disjoint entity chunks.
pub unsafe extern "C" fn host_par_for_each_chunk(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    let Some(task_cb) = chunk_fn else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: Validating `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }

    let Some(tables) = host_iter.tables else {
        return FfiResult::OK;
    };
    let plan = host_iter.plan;
    let last_run = host_iter.last_run;
    let this_run = host_iter.this_run;
    let send_world = UnsafeWorldCellSend(host_iter.world);
    let cb_ptr = GuestFnPtr(task_cb);
    let ud_ptr = GuestUserData::new(user_data);

    let first_err = AtomicU32::new(FfiResult::OK.0);
    let total_chunks = AtomicUsize::new(0);
    let total_entities = AtomicUsize::new(0);

    // Under Miri, run sequentially on the current thread to avoid spawning OS worker threads while
    // exercising the exact sub-range `HostIter` partitioning and `in_par_iter` invariants.
    let pool_opt: Option<&ComputeTaskPool> = if cfg!(miri) {
        None
    } else {
        Some(ComputeTaskPool::get_or_init(TaskPool::new))
    };
    let worker_count = pool_opt.map_or(1, |p| p.thread_num().max(1));

    match host_iter.backing {
        IterBacking::ResourceOnly => {
            host_iter.clear_active_chunk();
            return FfiResult::OK;
        }
        IterBacking::DenseTables {
            matched_tables,
            ref mut table_cursor,
            ref mut row_cursor,
            single_table_end_row,
        } => {
            let mut total_rows = 0usize;
            let start_tc = *table_cursor;
            let start_rc = *row_cursor;
            for &tid in &matched_tables[start_tc..] {
                if let Some(t) = tables.get(tid) {
                    let len = single_table_end_row
                        .map(|e| e.min(t.entity_count() as usize))
                        .unwrap_or(t.entity_count() as usize);
                    total_rows += len;
                }
            }
            *table_cursor = matched_tables.len();
            *row_cursor = 0;
            host_iter.clear_active_chunk();
            if total_rows == 0 {
                return FfiResult::OK;
            }

            let target_grain = (total_rows / (worker_count * 4).max(1)).max(256);
            let mut partitions = Vec::new();
            for (rel_idx, &tid) in matched_tables[start_tc..].iter().enumerate() {
                let Some(t) = tables.get(tid) else {
                    continue;
                };
                let full_len = t.entity_count() as usize;
                let end_limit = single_table_end_row
                    .map(|e| e.min(full_len))
                    .unwrap_or(full_len);
                let mut r = if rel_idx == 0 { start_rc } else { 0 };
                while r < end_limit {
                    let end = (r + target_grain).min(end_limit);
                    partitions.push(DenseParPartition {
                        table_id: tid,
                        start_row: r,
                        end_row: end,
                    });
                    r = end;
                }
            }

            let send_tables = TablesSend(tables);
            let run_partition = |part: DenseParPartition| {
                if first_err.load(Ordering::Acquire) != FfiResult::OK.0 {
                    return;
                }
                let st = send_tables;
                let sw = send_world;
                let tables_ref = st.0;
                let table_slice = core::slice::from_ref(&part.table_id);
                let mut sub_iter = HostIter {
                    header: HandleHeader::new(ITER_MAGIC),
                    tables: Some(tables_ref),
                    backing: IterBacking::DenseTables {
                        matched_tables: table_slice,
                        table_cursor: 0,
                        row_cursor: part.start_row,
                        single_table_end_row: Some(part.end_row),
                    },
                    plan,
                    last_run,
                    this_run,
                    world: sw.0,
                    sparse_scratch: None,
                    msg_cursors: None,
                    in_par_iter: true,
                    chunks_yielded: 0,
                    entities_yielded: 0,
                    current_entities_ptr: core::ptr::null(),
                    current_entity_count: 0,
                    marked_changed_mask: 0,
                    materialized_sparse_mask: 0,
                    present_sparse_mask: 0,
                    active_columns: [None; MAX_TERMS_PER_QUERY],
                    has_true_buf: Vec::new(),
                    has_false_buf: Vec::new(),
                };
                let GuestFnPtr(cb) = cb_ptr;
                // SAFETY: `sub_iter` owns a disjoint `[part.start_row..part.end_row]` slice of
                // `part.table_id` whose `iter_next_chunk` yields only its assigned sub-range and
                // poisons its `HandleHeader` on drop.
                let rc = unsafe { cb(sub_iter.as_ffi_ptr(), ud_ptr.as_ptr()) };
                if rc != FfiResult::OK {
                    let _ = first_err.compare_exchange(
                        FfiResult::OK.0,
                        rc.0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                let (c, e) = sub_iter.stats();
                total_chunks.fetch_add(c, Ordering::Relaxed);
                total_entities.fetch_add(e, Ordering::Relaxed);
            };

            if let Some(pool) = pool_opt
                && partitions.len() > 1
            {
                pool.scope(|s| {
                    for part in partitions {
                        s.spawn(async move {
                            run_partition(part);
                        });
                    }
                });
            } else {
                for part in partitions {
                    run_partition(part);
                }
            }
        }
        IterBacking::Archetypes {
            archetypes,
            sparse_sets,
            matched_archetypes,
            ref mut archetype_cursor,
            ref mut entity_cursor,
            single_arch_end_entity,
        } => {
            let start_ac = *archetype_cursor;
            let start_ec = *entity_cursor;
            let mut total_ents = 0usize;
            for &aid in &matched_archetypes[start_ac..] {
                if let Some(a) = archetypes.get(aid) {
                    let len = single_arch_end_entity
                        .map(|e| e.min(a.entities().len()))
                        .unwrap_or(a.entities().len());
                    total_ents += len;
                }
            }
            *archetype_cursor = matched_archetypes.len();
            *entity_cursor = 0;
            host_iter.clear_active_chunk();
            if total_ents == 0 {
                return FfiResult::OK;
            }

            let target_grain = (total_ents / (worker_count * 4).max(1)).max(256);
            let mut partitions = Vec::new();
            for (rel_idx, &aid) in matched_archetypes[start_ac..].iter().enumerate() {
                let Some(a) = archetypes.get(aid) else {
                    continue;
                };
                let full_len = a.entities().len();
                let end_limit = single_arch_end_entity
                    .map(|e| e.min(full_len))
                    .unwrap_or(full_len);
                let mut idx = if rel_idx == 0 { start_ec } else { 0 };
                while idx < end_limit {
                    let end = (idx + target_grain).min(end_limit);
                    partitions.push(ArchetypeParPartition {
                        arch_id: aid,
                        start_entity: idx,
                        end_entity: end,
                    });
                    idx = end;
                }
            }

            let send_tables = TablesSend(tables);
            let send_archetypes = ArchetypesSend(archetypes);
            let send_sparse_sets = SparseSetsSend(sparse_sets);
            let run_arch_partition = |part: ArchetypeParPartition| {
                if first_err.load(Ordering::Acquire) != FfiResult::OK.0 {
                    return;
                }
                let st = send_tables;
                let sa = send_archetypes;
                let ss = send_sparse_sets;
                let sw = send_world;
                let tables_ref = st.0;
                let archetypes_ref = sa.0;
                let sparse_sets_ref = ss.0;
                let mut local_scratch = SparseRowScratch::default();
                local_scratch.ensure_terms(plan.terms.len());
                let arch_slice = core::slice::from_ref(&part.arch_id);
                let mut sub_iter = HostIter {
                    header: HandleHeader::new(ITER_MAGIC),
                    tables: Some(tables_ref),
                    backing: IterBacking::Archetypes {
                        archetypes: archetypes_ref,
                        sparse_sets: sparse_sets_ref,
                        matched_archetypes: arch_slice,
                        archetype_cursor: 0,
                        entity_cursor: part.start_entity,
                        single_arch_end_entity: Some(part.end_entity),
                    },
                    plan,
                    last_run,
                    this_run,
                    world: sw.0,
                    sparse_scratch: Some(&mut local_scratch),
                    msg_cursors: None,
                    in_par_iter: true,
                    chunks_yielded: 0,
                    entities_yielded: 0,
                    current_entities_ptr: core::ptr::null(),
                    current_entity_count: 0,
                    marked_changed_mask: 0,
                    materialized_sparse_mask: 0,
                    present_sparse_mask: 0,
                    active_columns: [None; MAX_TERMS_PER_QUERY],
                    has_true_buf: Vec::new(),
                    has_false_buf: Vec::new(),
                };
                let GuestFnPtr(cb) = cb_ptr;
                // SAFETY: `sub_iter` owns a disjoint `[part.start_entity..part.end_entity]`
                // slice of `part.arch_id` with task-local `SparseRowScratch`, yielding only its
                // assigned sub-range via `iter_next_chunk`.
                let rc = unsafe { cb(sub_iter.as_ffi_ptr(), ud_ptr.as_ptr()) };
                if rc != FfiResult::OK {
                    let _ = first_err.compare_exchange(
                        FfiResult::OK.0,
                        rc.0,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    );
                }
                let (c, e) = sub_iter.stats();
                total_chunks.fetch_add(c, Ordering::Relaxed);
                total_entities.fetch_add(e, Ordering::Relaxed);
            };

            if let Some(pool) = pool_opt
                && partitions.len() > 1
            {
                pool.scope(|s| {
                    for part in partitions {
                        s.spawn(async move {
                            run_arch_partition(part);
                        });
                    }
                });
            } else {
                for part in partitions {
                    run_arch_partition(part);
                }
            }
        }
    }

    host_iter.chunks_yielded += total_chunks.load(Ordering::Relaxed);
    host_iter.entities_yielded += total_entities.load(Ordering::Relaxed);
    FfiResult(first_err.load(Ordering::Acquire))
}
