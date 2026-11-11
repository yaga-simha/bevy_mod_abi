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

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter};
use bevy_ecs::archetype::{ArchetypeId, Archetypes};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::storage::{SparseSets, Table, TableId, TableRow, Tables};
use core::cell::UnsafeCell;
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
    this_run: Tick,

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
        Self {
            tables,
            backing: IterBacking::DenseTables {
                matched_tables,
                table_cursor: 0,
            },
            query_columns,
            this_run,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            active_columns: [INIT_COL; MAX_QUERY_COMPONENTS],
        }
    }

    /// Creates an archetype-aware chunk iterator for queries involving at least one
    /// [`StorageType::SparseSet`](bevy_ecs::component::StorageType::SparseSet) component
    /// in either data access (`READ`/`WRITE`) or filters (`FILTER_ONLY`/`WITHOUT`).
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
            this_run,
            current_entities_ptr: core::ptr::null(),
            current_entity_count: 0,
            active_columns: [INIT_COL; MAX_QUERY_COMPONENTS],
        }
    }

    /// Advance to the next non-empty matching chunk.
    /// Returns 0 when all matching entities have been traversed.
    pub fn next_chunk(&mut self) -> usize {
        match self.backing {
            IterBacking::DenseTables {
                matched_tables,
                ref mut table_cursor,
            } => {
                while *table_cursor < matched_tables.len() {
                    let table_id = matched_tables[*table_cursor];
                    *table_cursor += 1;

                    let Some(table) = self.tables.get(table_id) else {
                        continue;
                    };

                    let count = table.entity_count() as usize;
                    if count == 0 {
                        // Skip empty tables so we don't prematurely terminate iteration
                        continue;
                    }

                    // Zero-copy cast: bevy_ecs Entity has #[repr(C, align(8))] matching FfiEntity(u64),
                    // and points directly into `&'w Table` outside `HostChunkIter`.
                    self.current_entities_ptr = table.entities().as_ptr().cast::<FfiEntity>();
                    self.current_entity_count = count;
                    self.populate_dense_table_columns(table, count);
                    return count;
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

                    let first = &arch_entities[*entity_cursor];
                    let first_entity = first.id();
                    let start_row = first.table_row();
                    let start_row_idx = start_row.index();

                    let run_len = if sparse_column_mask == 0 {
                        // All READ/WRITE data columns live in `Table`; coalesce contiguous TableRow runs!
                        let mut len = 1usize;
                        while *entity_cursor + len < arch_entities.len() {
                            if arch_entities[*entity_cursor + len].table_row().index()
                                == start_row_idx + len
                            {
                                len += 1;
                            } else {
                                break;
                            }
                        }
                        *entity_cursor += len;
                        len
                    } else {
                        // At least one READ/WRITE column lives in `ComponentSparseSet`; yield 1-row chunk.
                        *entity_cursor += 1;
                        1usize
                    };

                    let table_len = table.entity_count() as usize;
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
            IterBacking::DenseTables { table_cursor, .. } => {
                *table_cursor = 0;
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

    fn populate_dense_table_columns(&mut self, table: &'w Table, count: usize) {
        for col in &mut self.active_columns {
            *col = None;
        }

        for (i, q) in self
            .query_columns
            .iter()
            .enumerate()
            .take(MAX_QUERY_COMPONENTS)
        {
            if q.mode == ComponentAccessMode::FILTER_ONLY || q.mode == ComponentAccessMode::WITHOUT
            {
                self.active_columns[i] = None;
                continue;
            }

            if let Some(col) = table.get_column(q.component_id) {
                // SAFETY: table.entity_count() == count > 0, so TableRow 0 is in bounds.
                let data_ptr = unsafe {
                    col.get_data_unchecked(TableRow::new(NonMaxU32::ZERO))
                        .as_ptr() as *mut u8
                };
                // SAFETY: count is the valid entity count of the table.
                let changed_ticks = unsafe { col.get_changed_ticks_slice(count) };
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
            if q.mode == ComponentAccessMode::FILTER_ONLY || q.mode == ComponentAccessMode::WITHOUT
            {
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
    if host_iter.query_columns[index].mode == ComponentAccessMode::FILTER_ONLY
        || host_iter.query_columns[index].mode == ComponentAccessMode::WITHOUT
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
/// Automatically marks change ticks for all entities in the current chunk.
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
    // Strict permission check: only WRITE access is allowed to mutate.
    if host_iter.query_columns[index].mode != ComponentAccessMode::WRITE {
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
    if host_iter.query_columns[index].mode != ComponentAccessMode::WRITE {
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
