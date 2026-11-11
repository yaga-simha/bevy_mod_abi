#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Archetype & Table Chunk Traversal & Column Access
//!
//! This module implements [`HostChunkIter`] and the foreign C-ABI iteration API exposed
//! via [`RawChunkIter`].
//!
//! ## Key Principles
//! 1. **Zero-Allocation Dense & Archetypal Traversal**:
//!    - When all queried and filtered components use [`StorageType::Table`](bevy_ecs::component::StorageType::Table),
//!      [`HostChunkIter::new`] traverses matched [`Table`]s sequentially in dense columnar chunks,
//!      skipping empty tables (`entity_count == 0`).
//!    - When any queried or filtered component uses [`StorageType::SparseSet`](bevy_ecs::component::StorageType::SparseSet),
//!      [`HostChunkIter::new_archetypal`] traverses only matching [`Archetype`](bevy_ecs::archetype::Archetype)s:
//!      coalescing contiguous [`TableRow`] runs when all accessed data columns are `Table`-stored,
//!      or yielding zero-copy 1-row chunks directly from `Table` + `ComponentSparseSet` storage when
//!      any accessed data column is `SparseSet`-stored.
//! 2. **Access Mode Permission Enforcement**:
//!    - [`host_iter_column_readonly`]: Returns `*const u8`. Forbids access if column was
//!      declared `FILTER_ONLY` or `WITHOUT`.
//!    - [`host_iter_column_mutable`]: Returns `*mut u8`. Strictly verifies `mode == ComponentAccessMode::WRITE`.
//!      If a mod requested read access and attempts mutable access, it returns null.
//! 3. **Automatic & Fine-Grained Change Detection**:
//!    - Calling [`host_iter_column_mutable`] automatically marks all row ticks in `changed_ticks`
//!      and `summary_tick` as changed to `this_run`.
//!    - [`host_iter_mark_changed`] allows selective, row-by-row change marking for fine-grained updates.
//! 4. **Zero-Copy Entity Slices**:
//!    - Bevy's [`bevy_ecs::entity::Entity`] has `#[repr(C, align(8))]`, matching [`FfiEntity`].
//!    - [`host_iter_entities`] returns a direct zero-copy pointer into `&'w Table::entities()`,
//!      ensuring Stacked/Tree Borrows provenance stability across subsequent `&mut HostChunkIter` calls.
//! 5. **Iterator Reset**:
//!    - [`host_iter_reset`] rewinds traversal to the beginning, enabling efficient nested
//!      query loops with zero reallocations.

use crate::registry::ModRegistry;
use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter};
use bevy_ecs::archetype::{ArchetypeId, Archetypes};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::storage::{SparseSets, Table, TableId, TableRow, Tables};
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use core::cell::UnsafeCell;
use core::ffi::{c_char, CStr};
use nonmax::NonMaxU32;

/// Maximum number of component columns supported per single query.
pub const MAX_QUERY_COMPONENTS: usize = 32;

/// Descriptor of a queried component column inside a dynamic system.
#[derive(Clone, Copy, Debug)]
pub struct QueryColumnDesc {
    /// Bevy component ID assigned in the host world.
    pub component_id: ComponentId,
    /// Requested access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`).
    pub mode: ComponentAccessMode,
    /// Byte stride of the component type.
    pub stride: usize,
}

/// Active column state cached for the current chunk.
pub struct ActiveColumn<'w> {
    /// Pointer to the start of the column array in the current chunk.
    pub data_ptr: *mut u8,
    /// Slice of change detection ticks for rows in the current chunk.
    pub changed_ticks: &'w [UnsafeCell<Tick>],
    /// Summary change tick for the entire column in the current storage.
    pub summary_tick: Option<&'w AtomicTick>,
    /// Byte stride between consecutive elements in the column.
    pub stride: usize,
    /// Declared access mode for this column.
    pub mode: ComponentAccessMode,
}

enum IterBacking<'w> {
    DenseTables {
        matched_tables: &'w [TableId],
        table_cursor: usize,
        row_cursor: usize,
    },
    Archetypes {
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        archetype_cursor: usize,
        entity_cursor: usize,
        sparse_column_mask: u32,
    },
}

/// Host-side concrete chunk iterator backing the opaque [`RawChunkIter`] handle.
pub struct HostChunkIter<'w> {
    tables: &'w Tables,
    backing: IterBacking<'w>,
    query_columns: &'w [QueryColumnDesc],
    last_run: Tick,
    this_run: Tick,
    has_tick_filters: bool,

    world: Option<UnsafeWorldCell<'w>>,
    registry: Option<&'w ModRegistry>,
    declared_resources: &'w [(ComponentId, bool)],

    current_entities_ptr: *const FfiEntity,
    current_entity_count: usize,
    active_columns: [Option<ActiveColumn<'w>>; MAX_QUERY_COMPONENTS],
}

impl<'w> HostChunkIter<'w> {
    /// Creates a new dense table chunk iterator for queries whose accessed and filtered
    /// components all use [`StorageType::Table`](bevy_ecs::component::StorageType::Table).
    pub fn new(
        tables: &'w Tables,
        matched_tables: &'w [TableId],
        query_columns: &'w [QueryColumnDesc],
        this_run: Tick,
    ) -> Self {
        const INIT_COL: Option<ActiveColumn<'static>> = None;
        let has_tick_filters = query_columns.iter().any(|c| c.mode.is_tick_filter());
        let last_run = Tick::new(this_run.get().wrapping_sub(Tick::MAX.get()));
        Self {
            tables,
            backing: IterBacking::DenseTables {
                matched_tables,
                table_cursor: 0,
                row_cursor: 0,
            },
            query_columns,
            last_run,
            this_run,
            has_tick_filters,
            world: None,
            registry: None,
            declared_resources: &[],
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            active_columns: [INIT_COL; MAX_QUERY_COMPONENTS],
        }
    }

    /// Creates an archetype-aware chunk iterator for queries involving at least one
    /// [`StorageType::SparseSet`](bevy_ecs::component::StorageType::SparseSet) component
    /// in either data access (`READ`/`WRITE`/`OPTIONAL_*`) or filters (`FILTER_ONLY`/`WITHOUT`/`CHANGED`/`ADDED`).
    ///
    /// - `sparse_column_mask`: bit `i` is set (`1 << i`) iff `query_columns[i]` is stored in
    ///   `SparseSets` rather than `Tables`.
    pub fn new_archetypal(
        tables: &'w Tables,
        archetypes: &'w Archetypes,
        sparse_sets: &'w SparseSets,
        matched_archetypes: &'w [ArchetypeId],
        query_columns: &'w [QueryColumnDesc],
        sparse_column_mask: u32,
        this_run: Tick,
    ) -> Self {
        const INIT_COL: Option<ActiveColumn<'static>> = None;
        let has_tick_filters = query_columns.iter().any(|c| c.mode.is_tick_filter());
        let last_run = Tick::new(this_run.get().wrapping_sub(Tick::MAX.get()));
        Self {
            tables,
            backing: IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                archetype_cursor: 0,
                entity_cursor: 0,
                sparse_column_mask,
            },
            query_columns,
            last_run,
            this_run,
            has_tick_filters,
            world: None,
            registry: None,
            declared_resources: &[],
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            active_columns: [INIT_COL; MAX_QUERY_COMPONENTS],
        }
    }

    /// Sets the system's previous run tick (`last_run`) used to evaluate `Changed<T>` and `Added<T>`.
    #[inline]
    pub fn with_last_run(mut self, last_run: Tick) -> Self {
        self.last_run = last_run;
        self
    }

    /// Attaches singleton Bevy `Resource` access context (`UnsafeWorldCell` + declared permissions).
    #[inline]
    pub fn with_resources(
        mut self,
        world: UnsafeWorldCell<'w>,
        registry: &'w ModRegistry,
        declared_resources: &'w [(ComponentId, bool)],
    ) -> Self {
        self.world = Some(world);
        self.registry = Some(registry);
        self.declared_resources = declared_resources;
        self
    }

    #[inline]
    fn row_matches_dense_tick_filters(
        table: &'w Table,
        table_len: usize,
        row: usize,
        query_columns: &'w [QueryColumnDesc],
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        for q in query_columns {
            match q.mode {
                ComponentAccessMode::CHANGED => {
                    let Some(col) = table.get_column(q.component_id) else {
                        return false;
                    };
                    // SAFETY: `table_len == table.entity_count() as usize` and `row < table_len`.
                    let ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                    // SAFETY: Read-only access to component ticks was declared in `FilteredAccess`.
                    let tick = unsafe { *ticks[row].get() };
                    if !tick.is_newer_than(last_run, this_run) {
                        return false;
                    }
                }
                ComponentAccessMode::ADDED => {
                    let Some(col) = table.get_column(q.component_id) else {
                        return false;
                    };
                    // SAFETY: `table_len == table.entity_count() as usize` and `row < table_len`.
                    let ticks = unsafe { col.get_added_ticks_slice(table_len) };
                    // SAFETY: Read-only access to component ticks was declared in `FilteredAccess`.
                    let tick = unsafe { *ticks[row].get() };
                    if !tick.is_newer_than(last_run, this_run) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }

    #[inline]
    #[allow(clippy::too_many_arguments)]
    fn entity_matches_archetypal_tick_filters(
        table: &'w Table,
        table_len: usize,
        sparse_sets: &'w SparseSets,
        entity: bevy_ecs::entity::Entity,
        table_row_idx: usize,
        query_columns: &'w [QueryColumnDesc],
        last_run: Tick,
        this_run: Tick,
    ) -> bool {
        for q in query_columns {
            match q.mode {
                ComponentAccessMode::CHANGED => {
                    let tick = if let Some(col) = table.get_column(q.component_id) {
                        if table_row_idx >= table_len {
                            return false;
                        }
                        // SAFETY: `table_len == table.entity_count() as usize` and `table_row_idx < table_len`.
                        let ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                        // SAFETY: Read-only access to component ticks was declared in `FilteredAccess`.
                        unsafe { *ticks[table_row_idx].get() }
                    } else if let Some(sparse_set) = sparse_sets.get(q.component_id) {
                        let Some(tick_cell) = sparse_set.get_changed_tick(entity) else {
                            return false;
                        };
                        // SAFETY: Read-only access to sparse component ticks was declared in `FilteredAccess`.
                        unsafe { *tick_cell.get() }
                    } else {
                        return false;
                    };
                    if !tick.is_newer_than(last_run, this_run) {
                        return false;
                    }
                }
                ComponentAccessMode::ADDED => {
                    let tick = if let Some(col) = table.get_column(q.component_id) {
                        if table_row_idx >= table_len {
                            return false;
                        }
                        // SAFETY: `table_len == table.entity_count() as usize` and `table_row_idx < table_len`.
                        let ticks = unsafe { col.get_added_ticks_slice(table_len) };
                        // SAFETY: Read-only access to component ticks was declared in `FilteredAccess`.
                        unsafe { *ticks[table_row_idx].get() }
                    } else if let Some(sparse_set) = sparse_sets.get(q.component_id) {
                        let Some(tick_cell) = sparse_set.get_added_tick(entity) else {
                            return false;
                        };
                        // SAFETY: Read-only access to sparse component ticks was declared in `FilteredAccess`.
                        unsafe { *tick_cell.get() }
                    } else {
                        return false;
                    };
                    if !tick.is_newer_than(last_run, this_run) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        true
    }

    /// Advance to the next non-empty matching chunk.
    /// Returns 0 when all matching entities have been traversed.
    pub fn next_chunk(&mut self) -> usize {
        let has_tick_filters = self.has_tick_filters;
        let last_run = self.last_run;
        let this_run = self.this_run;
        let query_columns = self.query_columns;

        match self.backing {
            IterBacking::DenseTables {
                matched_tables,
                ref mut table_cursor,
                ref mut row_cursor,
            } => {
                while *table_cursor < matched_tables.len() {
                    let table_id = matched_tables[*table_cursor];
                    let Some(table) = self.tables.get(table_id) else {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    };

                    let count = table.entity_count() as usize;
                    if count == 0 || *row_cursor >= count {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    if !has_tick_filters {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        self.current_entities_ptr = table.entities().as_ptr().cast::<FfiEntity>();
                        self.current_entity_count = count;
                        self.populate_dense_table_range_columns(table, count, 0, count);
                        return count;
                    }

                    // Scan from `*row_cursor` for the next contiguous run of rows matching `CHANGED`/`ADDED`
                    let mut start_row_idx = *row_cursor;
                    while start_row_idx < count
                        && !Self::row_matches_dense_tick_filters(
                            table,
                            count,
                            start_row_idx,
                            query_columns,
                            last_run,
                            this_run,
                        )
                    {
                        start_row_idx += 1;
                    }

                    if start_row_idx >= count {
                        *table_cursor += 1;
                        *row_cursor = 0;
                        continue;
                    }

                    let mut run_len = 1usize;
                    while start_row_idx + run_len < count
                        && Self::row_matches_dense_tick_filters(
                            table,
                            count,
                            start_row_idx + run_len,
                            query_columns,
                            last_run,
                            this_run,
                        )
                    {
                        run_len += 1;
                    }

                    *row_cursor = start_row_idx + run_len;
                    if *row_cursor >= count {
                        *table_cursor += 1;
                        *row_cursor = 0;
                    }

                    // SAFETY: `start_row_idx + run_len <= count == table.entities().len()`.
                    self.current_entities_ptr = unsafe {
                        table
                            .entities()
                            .as_ptr()
                            .add(start_row_idx)
                            .cast::<FfiEntity>()
                    };
                    self.current_entity_count = run_len;
                    self.populate_dense_table_range_columns(table, count, start_row_idx, run_len);
                    return run_len;
                }
            }
            IterBacking::Archetypes {
                archetypes,
                sparse_sets,
                matched_archetypes,
                ref mut archetype_cursor,
                ref mut entity_cursor,
                sparse_column_mask,
            } => {
                while *archetype_cursor < matched_archetypes.len() {
                    let arch_id = matched_archetypes[*archetype_cursor];
                    let Some(archetype) = archetypes.get(arch_id) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };

                    let arch_entities = archetype.entities();
                    if *entity_cursor >= arch_entities.len() {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    }

                    let Some(table) = self.tables.get(archetype.table_id()) else {
                        *archetype_cursor += 1;
                        *entity_cursor = 0;
                        continue;
                    };
                    let table_len = table.entity_count() as usize;

                    if has_tick_filters {
                        while *entity_cursor < arch_entities.len() {
                            let cand = &arch_entities[*entity_cursor];
                            if Self::entity_matches_archetypal_tick_filters(
                                table,
                                table_len,
                                sparse_sets,
                                cand.id(),
                                cand.table_row().index(),
                                query_columns,
                                last_run,
                                this_run,
                            ) {
                                break;
                            }
                            *entity_cursor += 1;
                        }
                        if *entity_cursor >= arch_entities.len() {
                            *archetype_cursor += 1;
                            *entity_cursor = 0;
                            continue;
                        }
                    }

                    let first = &arch_entities[*entity_cursor];
                    let first_entity = first.id();
                    let start_row = first.table_row();
                    let start_row_idx = start_row.index();

                    let run_len = if sparse_column_mask == 0 {
                        // All READ/WRITE/OPTIONAL data columns live in `Table`; coalesce contiguous TableRow runs!
                        let mut len = 1usize;
                        while *entity_cursor + len < arch_entities.len() {
                            let next_ent = &arch_entities[*entity_cursor + len];
                            if next_ent.table_row().index() != start_row_idx + len {
                                break;
                            }
                            if has_tick_filters
                                && !Self::entity_matches_archetypal_tick_filters(
                                    table,
                                    table_len,
                                    sparse_sets,
                                    next_ent.id(),
                                    start_row_idx + len,
                                    query_columns,
                                    last_run,
                                    this_run,
                                )
                            {
                                break;
                            }
                            len += 1;
                        }
                        *entity_cursor += len;
                        len
                    } else {
                        // At least one accessed column lives in `ComponentSparseSet`; yield 1-row chunk.
                        *entity_cursor += 1;
                        1usize
                    };

                    if start_row_idx + run_len > table_len {
                        continue;
                    }

                    // SAFETY: Every entity in `archetype` has an allocated `TableRow` in `archetype.table_id()`,
                    // and `start_row_idx + run_len <= table.entities().len()` was verified above.
                    // `Entity` is `#[repr(C, align(8))]`, layout-identical to `FfiEntity(u64)`.
                    self.current_entities_ptr = unsafe {
                        table
                            .entities()
                            .as_ptr()
                            .add(start_row_idx)
                            .cast::<FfiEntity>()
                    };
                    self.current_entity_count = run_len;
                    self.populate_archetypal_columns(
                        table,
                        table_len,
                        sparse_sets,
                        first_entity,
                        start_row,
                        start_row_idx,
                        run_len,
                        sparse_column_mask,
                    );
                    return run_len;
                }
            }
        }

        // Exhausted
        self.clear_active_chunk();
        0
    }

    /// Reset this iterator back to the first matched chunk so nested queries
    /// can re-traverse chunks with zero allocations.
    #[inline]
    pub fn reset(&mut self) {
        match &mut self.backing {
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

    #[inline]
    fn clear_active_chunk(&mut self) {
        self.current_entities_ptr = core::ptr::null();
        self.current_entity_count = 0;
        for col in &mut self.active_columns {
            *col = None;
        }
    }

    #[inline]
    fn column_is_non_data(&self, q: &QueryColumnDesc) -> bool {
        if q.mode == ComponentAccessMode::FILTER_ONLY
            || q.mode == ComponentAccessMode::WITHOUT
            || q.mode.is_resource()
        {
            return true;
        }
        // When `CHANGED(T)` / `ADDED(T)` coexists with `WRITE(T)` / `OPTIONAL_WRITE(T)` in the
        // same query group, treat the filter column as pure tick-filter metadata so callers cannot
        // simultaneously borrow `&T` (filter col) and `&mut T` (write col).
        if q.mode.is_tick_filter() {
            return self.query_columns.iter().any(|other| {
                other.component_id == q.component_id
                    && (other.mode == ComponentAccessMode::WRITE
                        || other.mode == ComponentAccessMode::OPTIONAL_WRITE)
            });
        }
        false
    }

    fn populate_dense_table_range_columns(
        &mut self,
        table: &'w Table,
        table_len: usize,
        start_row_idx: usize,
        run_len: usize,
    ) {
        for col in &mut self.active_columns {
            *col = None;
        }

        let Some(start_nonmax) = NonMaxU32::new(start_row_idx as u32) else {
            return;
        };
        let start_row = TableRow::new(start_nonmax);

        for (i, q) in self
            .query_columns
            .iter()
            .enumerate()
            .take(MAX_QUERY_COMPONENTS)
        {
            if self.column_is_non_data(q) {
                self.active_columns[i] = None;
                continue;
            }

            if let Some(col) = table.get_column(q.component_id) {
                // SAFETY: `start_row_idx + run_len <= table_len == table.entity_count() as usize`.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr() as *mut u8 };
                // SAFETY: `table_len` is the valid entity count of `table`.
                let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                let changed_ticks = &all_ticks[start_row_idx..start_row_idx + run_len];
                let summary_tick = col.get_summary_tick();

                self.active_columns[i] = Some(ActiveColumn {
                    data_ptr,
                    changed_ticks,
                    summary_tick,
                    stride: q.stride,
                    mode: q.mode,
                });
            } else {
                self.active_columns[i] = None;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn populate_archetypal_columns(
        &mut self,
        table: &'w Table,
        table_len: usize,
        sparse_sets: &'w SparseSets,
        first_entity: bevy_ecs::entity::Entity,
        start_row: TableRow,
        start_row_idx: usize,
        run_len: usize,
        sparse_column_mask: u32,
    ) {
        for col in &mut self.active_columns {
            *col = None;
        }

        for (i, q) in self
            .query_columns
            .iter()
            .enumerate()
            .take(MAX_QUERY_COMPONENTS)
        {
            if self.column_is_non_data(q) {
                self.active_columns[i] = None;
                continue;
            }

            let is_sparse_col = (sparse_column_mask & (1u32 << i)) != 0;
            if is_sparse_col {
                debug_assert_eq!(run_len, 1);
                if let Some(sparse_set) = sparse_sets.get(q.component_id) {
                    if let Some((ptr, tick_cells)) = sparse_set.get_with_ticks(first_entity) {
                        self.active_columns[i] = Some(ActiveColumn {
                            data_ptr: ptr.as_ptr() as *mut u8,
                            changed_ticks: core::slice::from_ref(tick_cells.changed),
                            summary_tick: tick_cells.summary_tick,
                            stride: q.stride,
                            mode: q.mode,
                        });
                    }
                }
            } else if let Some(col) = table.get_column(q.component_id) {
                // SAFETY: `start_row_idx + run_len <= table_len` was verified by caller, so
                // `start_row` is in bounds and `[start_row_idx..start_row_idx + run_len]` is valid.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr() as *mut u8 };
                // SAFETY: `table_len` is `table.entity_count() as usize`.
                let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                let changed_ticks = &all_ticks[start_row_idx..start_row_idx + run_len];
                let summary_tick = col.get_summary_tick();

                self.active_columns[i] = Some(ActiveColumn {
                    data_ptr,
                    changed_ticks,
                    summary_tick,
                    stride: q.stride,
                    mode: q.mode,
                });
            }
        }
    }
}

/// Advance `RawChunkIter` to the next chunk. Returns chunk entity count or 0 when done.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_next_chunk(iter: *mut RawChunkIter) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &mut *(iter as *mut HostChunkIter<'_>) };
    host_iter.next_chunk()
}

/// Borrow a read-only column byte pointer from `RawChunkIter`.
///
/// Returns `null` when an `OPTIONAL_READ` / `OPTIONAL_WRITE` column is absent on the current
/// archetype, or when `index` refers to a `FILTER_ONLY`, `WITHOUT`, or `RESOURCE_*` entry.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_column_readonly(
    iter: *const RawChunkIter,
    index: usize,
) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &*(iter as *const HostChunkIter<'_>) };
    if host_iter.current_entity_count == 0
        || index >= host_iter.query_columns.len()
        || index >= MAX_QUERY_COMPONENTS
    {
        return core::ptr::null();
    }
    let mode = host_iter.query_columns[index].mode;
    if mode == ComponentAccessMode::FILTER_ONLY
        || mode == ComponentAccessMode::WITHOUT
        || mode.is_resource()
    {
        return core::ptr::null();
    }
    if let Some(col) = &host_iter.active_columns[index] {
        col.data_ptr as *const u8
    } else {
        core::ptr::null()
    }
}

/// Borrow a mutable column byte pointer from `RawChunkIter`.
/// Automatically marks change ticks for all entities in the current chunk when present.
/// Returns `null` when an `OPTIONAL_WRITE` column is absent on the current archetype,
/// or if `index` was not declared with `WRITE` or `OPTIONAL_WRITE`.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_column_mutable(
    iter: *mut RawChunkIter,
    index: usize,
) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &mut *(iter as *mut HostChunkIter<'_>) };
    if host_iter.current_entity_count == 0
        || index >= host_iter.query_columns.len()
        || index >= MAX_QUERY_COMPONENTS
    {
        return core::ptr::null_mut();
    }
    let mode = host_iter.query_columns[index].mode;
    if mode != ComponentAccessMode::WRITE && mode != ComponentAccessMode::OPTIONAL_WRITE {
        return core::ptr::null_mut();
    }

    if let Some(col) = &mut host_iter.active_columns[index] {
        for tick_cell in col.changed_ticks {
            // SAFETY: System holds exclusive access to this component's rows as granted by Bevy scheduler.
            unsafe {
                *tick_cell.get() = host_iter.this_run;
            }
        }
        if let Some(summary) = col.summary_tick {
            summary.set(host_iter.this_run);
        }
        col.data_ptr
    } else {
        core::ptr::null_mut()
    }
}

/// Mark an individual row's component tick as changed.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_mark_changed(
    iter: *mut RawChunkIter,
    index: usize,
    row: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &mut *(iter as *mut HostChunkIter<'_>) };
    if index >= host_iter.query_columns.len() || index >= MAX_QUERY_COMPONENTS {
        return FfiResult::INVALID_COMPONENT;
    }
    let mode = host_iter.query_columns[index].mode;
    if mode != ComponentAccessMode::WRITE && mode != ComponentAccessMode::OPTIONAL_WRITE {
        return FfiResult::PERMISSION_DENIED;
    }
    if row >= host_iter.current_entity_count {
        return FfiResult::INTERNAL_ERROR;
    }

    if let Some(col) = &mut host_iter.active_columns[index] {
        // SAFETY: row < current_entity_count == changed_ticks.len()
        unsafe {
            *col.changed_ticks[row].get() = host_iter.this_run;
        }
        if let Some(summary) = col.summary_tick {
            summary.set(host_iter.this_run);
        }
        FfiResult::OK
    } else {
        FfiResult::INVALID_COMPONENT
    }
}

/// Borrow the contiguous `FfiEntity` slice for the current chunk.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_entities(iter: *const RawChunkIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &*(iter as *const HostChunkIter<'_>) };
    if host_iter.current_entity_count == 0 {
        return core::ptr::null();
    }
    host_iter.current_entities_ptr
}

/// Query the byte stride of column `index` in `RawChunkIter`.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_column_stride(iter: *const RawChunkIter, index: usize) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &*(iter as *const HostChunkIter<'_>) };
    if index >= host_iter.query_columns.len() {
        return 0;
    }
    host_iter.query_columns[index].stride
}

/// Reset a `RawChunkIter` back to its first matched chunk for nested query iteration.
///
/// # Safety
/// `iter` must be a valid pointer to a `HostChunkIter` or null.
pub unsafe extern "C" fn host_iter_reset(iter: *mut RawChunkIter) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: caller passes a valid pointer to HostChunkIter cast to RawChunkIter.
    let host_iter = unsafe { &mut *(iter as *mut HostChunkIter<'_>) };
    host_iter.reset();
    FfiResult::OK
}

/// Borrow a read-only pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Enforces that `resource_name` is registered as a resource (`register_mod_resource`) AND was
/// declared on the executing system with `RESOURCE_READ` or `RESOURCE_WRITE`.
///
/// # Safety
/// - `iter` must be a valid pointer to an active `HostChunkIter` or null.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_iter_resource_readonly(
    iter: *const RawChunkIter,
    resource_name: *const c_char,
) -> *const u8 {
    if iter.is_null() || resource_name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: caller passes a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(resource_name) }).to_str() else {
        return core::ptr::null();
    };
    // SAFETY: caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &*(iter as *const HostChunkIter<'_>) };
    let (Some(world), Some(registry)) = (host_iter.world, host_iter.registry) else {
        return core::ptr::null();
    };
    let Some(reg_comp) = registry.get_by_name(name) else {
        return core::ptr::null();
    };
    if !reg_comp.is_resource {
        return core::ptr::null();
    }
    if !host_iter
        .declared_resources
        .iter()
        .any(|&(cid, _)| cid == reg_comp.component_id)
    {
        return core::ptr::null();
    }

    // SAFETY: Shared read access (or exclusive write access) to `reg_comp.component_id` was
    // declared in `FilteredAccessSet` during `DynamicCSystem::initialize`.
    match unsafe { world.get_resource_by_id(reg_comp.component_id) } {
        Some(ptr) => ptr.as_ptr(),
        None => core::ptr::null(),
    }
}

/// Borrow an exclusive mutable pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Enforces that `resource_name` is registered as a resource (`register_mod_resource`) AND was
/// declared strictly with `RESOURCE_WRITE` on the executing system. Automatically marks the
/// resource's Bevy change tick to `this_run` (`Res<R>::is_changed()` becomes `true`).
///
/// # Safety
/// - `iter` must be a valid pointer to an active `HostChunkIter` or null.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_iter_resource_mutable(
    iter: *mut RawChunkIter,
    resource_name: *const c_char,
) -> *mut u8 {
    if iter.is_null() || resource_name.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: caller passes a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(resource_name) }).to_str() else {
        return core::ptr::null_mut();
    };
    // SAFETY: caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter as *mut HostChunkIter<'_>) };
    let (Some(world), Some(registry)) = (host_iter.world, host_iter.registry) else {
        return core::ptr::null_mut();
    };
    let Some(reg_comp) = registry.get_by_name(name) else {
        return core::ptr::null_mut();
    };
    if !reg_comp.is_resource {
        return core::ptr::null_mut();
    }
    if !host_iter
        .declared_resources
        .iter()
        .any(|&(cid, writable)| cid == reg_comp.component_id && writable)
    {
        return core::ptr::null_mut();
    }

    // SAFETY: `RESOURCE_WRITE` was declared in `FilteredAccessSet` during `DynamicCSystem::initialize`,
    // granting exclusive mutable access to `reg_comp.component_id` and its resource entity.
    let Some(entity) = (unsafe { world.resource_entities() }).get(reg_comp.component_id) else {
        return core::ptr::null_mut();
    };
    let Ok(entity_cell) =
        world.get_entity_with_ticks(entity, host_iter.last_run, host_iter.this_run)
    else {
        return core::ptr::null_mut();
    };
    // SAFETY: Exclusive mutable access to this resource component is guaranteed by Bevy's scheduler.
    let Ok(mut_untyped) = (unsafe { entity_cell.get_mut_by_id(reg_comp.component_id) }) else {
        return core::ptr::null_mut();
    };
    mut_untyped.into_inner().as_ptr()
}
