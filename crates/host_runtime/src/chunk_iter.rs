#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use abi_contract::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::storage::{Table, TableId, TableRow, Tables};
use core::cell::UnsafeCell;
use nonmax::NonMaxU32;

pub const MAX_QUERY_COMPONENTS: usize = 32;

/// Descriptor of a queried component column inside a dynamic system.
#[derive(Clone, Copy, Debug)]
pub struct QueryColumnDesc {
    pub component_id: ComponentId,
    pub mode: ComponentAccessMode,
    pub stride: usize,
}

/// Active column state cached for the current chunk.
pub struct ActiveColumn<'w> {
    pub data_ptr: *mut u8,
    pub changed_ticks: &'w [UnsafeCell<Tick>],
    pub summary_tick: Option<&'w AtomicTick>,
    pub stride: usize,
    pub mode: ComponentAccessMode,
}

/// Host-side concrete chunk iterator backing the opaque `RawChunkIter` handle.
pub struct HostChunkIter<'w> {
    tables: &'w Tables,
    matched_tables: &'w [TableId],
    current_table_cursor: usize,
    query_columns: &'w [QueryColumnDesc],
    this_run: Tick,

    current_table: Option<&'w Table>,
    current_entity_count: usize,
    active_columns: [Option<ActiveColumn<'w>>; MAX_QUERY_COMPONENTS],
}

impl<'w> HostChunkIter<'w> {
    pub fn new(
        tables: &'w Tables,
        matched_tables: &'w [TableId],
        query_columns: &'w [QueryColumnDesc],
        this_run: Tick,
    ) -> Self {
        const INIT_COL: Option<ActiveColumn<'static>> = None;
        Self {
            tables,
            matched_tables,
            current_table_cursor: 0,
            query_columns,
            this_run,
            current_table: None,
            current_entity_count: 0,
            active_columns: [INIT_COL; MAX_QUERY_COMPONENTS],
        }
    }

    /// Advance to the next non-empty matching table.
    /// Returns 0 when all matching tables have been traversed.
    pub fn next_chunk(&mut self) -> usize {
        while self.current_table_cursor < self.matched_tables.len() {
            let table_id = self.matched_tables[self.current_table_cursor];
            self.current_table_cursor += 1;

            let Some(table) = self.tables.get(table_id) else {
                continue;
            };

            let count = table.entity_count() as usize;
            if count == 0 {
                // Skip empty tables so we don't prematurely terminate iteration
                continue;
            }

            self.current_table = Some(table);
            self.current_entity_count = count;
            self.populate_active_columns(table, count);
            return count;
        }

        // Exhausted
        self.current_table = None;
        self.current_entity_count = 0;
        for col in &mut self.active_columns {
            *col = None;
        }
        0
    }

    /// Reset this iterator back to the first matched archetype table so nested queries
    /// can re-traverse chunks with zero allocations.
    #[inline]
    pub fn reset(&mut self) {
        self.current_table_cursor = 0;
        self.current_table = None;
        self.current_entity_count = 0;
        for col in &mut self.active_columns {
            *col = None;
        }
    }

    fn populate_active_columns(&mut self, table: &'w Table, count: usize) {
        for col in &mut self.active_columns {
            *col = None;
        }

        for (i, q) in self.query_columns.iter().enumerate() {
            if i >= MAX_QUERY_COMPONENTS {
                break;
            }
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
                // SAFETY: count is valid entity count of the table.
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
}

/// Advance `RawChunkIter` to the next table chunk. Returns chunk entity count or 0 when done.
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
            // SAFETY: System holds exclusive access to this component as granted by Bevy scheduler.
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
        // SAFETY: row < current_entity_count <= changed_ticks.len()
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
    if let Some(table) = host_iter.current_table {
        // Zero-copy cast: bevy_ecs Entity has #[repr(C, align(8))] matching FfiEntity(u64)
        table.entities().as_ptr() as *const FfiEntity
    } else {
        core::ptr::null()
    }
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

/// Reset a `RawChunkIter` back to its first matched archetype table for nested query iteration.
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
