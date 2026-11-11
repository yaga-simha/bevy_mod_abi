//! Archetype and table chunk traversal and C-ABI column access functions.

use crate::registry::ModRegistry;
use bevy_abi_mod_sys::{ComponentAccessMode, FfiEntity, FfiResult, RawChunkIter};
use bevy_ecs::archetype::{ArchetypeId, Archetypes};
use bevy_ecs::change_detection::{AtomicTick, Tick};
use bevy_ecs::component::ComponentId;
use bevy_ecs::entity::Entity;
use bevy_ecs::storage::{SparseSets, Table, TableId, TableRow, Tables};
use bevy_ecs::world::unsafe_world_cell::UnsafeWorldCell;
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_char};
use nonmax::NonMaxU32;

// Compile-time layout assertions guarding zero-copy `Entity` -> `FfiEntity` slice casts.
const _: () = assert!(core::mem::size_of::<Entity>() == core::mem::size_of::<FfiEntity>());
const _: () = assert!(core::mem::align_of::<Entity>() == core::mem::align_of::<FfiEntity>());

/// Maximum number of component columns supported per single query.
pub const MAX_QUERY_COMPONENTS: usize = 32;

/// Descriptor of a queried component column inside a dynamic system.
#[derive(Clone, Copy, Debug)]
pub struct QueryColumnDesc {
    /// Bevy component ID assigned in the host world.
    pub component_id: ComponentId,
    /// Requested access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`, etc.).
    pub mode: ComponentAccessMode,
    /// Byte stride of the component type.
    pub stride: usize,
}

/// Active column state cached for the current chunk.
struct ActiveColumn<'w> {
    data_ptr: *mut u8,
    changed_ticks: &'w [UnsafeCell<Tick>],
    summary_tick: Option<&'w AtomicTick>,
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
    /// Creates a dense table chunk iterator for queries whose accessed and filtered components all
    /// use [`StorageType::Table`](bevy_ecs::component::StorageType::Table).
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
    /// [`StorageType::SparseSet`](bevy_ecs::component::StorageType::SparseSet) component.
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
        entity: Entity,
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

    /// Advances to the next non-empty matching chunk, returning 0 when exhausted.
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
                        *entity_cursor += 1;
                        1usize
                    };

                    if start_row_idx + run_len > table_len {
                        continue;
                    }

                    // SAFETY: `start_row_idx + run_len <= table.entities().len()`, and `Entity`
                    // matches the size, alignment, and `to_bits()` bit-layout of `FfiEntity`.
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

        self.clear_active_chunk();
        0
    }

    /// Resets this iterator back to the first matched chunk.
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
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr().cast_mut() };
                // SAFETY: `table_len` is the valid entity count of `table`.
                let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                let changed_ticks = &all_ticks[start_row_idx..start_row_idx + run_len];
                let summary_tick = col.get_summary_tick();

                self.active_columns[i] = Some(ActiveColumn {
                    data_ptr,
                    changed_ticks,
                    summary_tick,
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
        first_entity: Entity,
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
                if let Some(sparse_set) = sparse_sets.get(q.component_id)
                    && let Some((ptr, tick_cells)) = sparse_set.get_with_ticks(first_entity)
                {
                    self.active_columns[i] = Some(ActiveColumn {
                        data_ptr: ptr.as_ptr().cast_mut(),
                        changed_ticks: core::slice::from_ref(tick_cells.changed),
                        summary_tick: tick_cells.summary_tick,
                    });
                }
            } else if let Some(col) = table.get_column(q.component_id) {
                // SAFETY: `start_row_idx + run_len <= table_len` was verified by caller.
                let data_ptr = unsafe { col.get_data_unchecked(start_row).as_ptr().cast_mut() };
                // SAFETY: `table_len` is `table.entity_count() as usize`.
                let all_ticks = unsafe { col.get_changed_ticks_slice(table_len) };
                let changed_ticks = &all_ticks[start_row_idx..start_row_idx + run_len];
                let summary_tick = col.get_summary_tick();

                self.active_columns[i] = Some(ActiveColumn {
                    data_ptr,
                    changed_ticks,
                    summary_tick,
                });
            }
        }
    }
}

/// Advances `RawChunkIter` to the next chunk, returning chunk entity count or 0 when done.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_next_chunk(iter: *mut RawChunkIter) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter.cast::<HostChunkIter<'_>>()) };
    host_iter.next_chunk()
}

/// Borrows a read-only column byte pointer from `RawChunkIter`.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_column_readonly(
    iter: *const RawChunkIter,
    index: usize,
) -> *const u8 {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &*(iter.cast::<HostChunkIter<'_>>()) };
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
        col.data_ptr.cast_const()
    } else {
        core::ptr::null()
    }
}

/// Borrows a mutable column byte pointer from `RawChunkIter` and marks chunk ticks changed.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_column_mutable(
    iter: *mut RawChunkIter,
    index: usize,
) -> *mut u8 {
    if iter.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter.cast::<HostChunkIter<'_>>()) };
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
            // SAFETY: System holds exclusive access to this component's rows as granted by Bevy's scheduler.
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

/// Marks an individual row's component tick as changed.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_mark_changed(
    iter: *mut RawChunkIter,
    index: usize,
    row: usize,
) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter.cast::<HostChunkIter<'_>>()) };
    if index >= host_iter.query_columns.len() || index >= MAX_QUERY_COMPONENTS {
        return FfiResult::INVALID_COMPONENT;
    }
    let mode = host_iter.query_columns[index].mode;
    if mode != ComponentAccessMode::WRITE && mode != ComponentAccessMode::OPTIONAL_WRITE {
        return FfiResult::PERMISSION_DENIED;
    }
    if row >= host_iter.current_entity_count {
        return FfiResult::OUT_OF_RANGE;
    }

    if let Some(col) = &mut host_iter.active_columns[index] {
        // SAFETY: `row < current_entity_count == changed_ticks.len()`, and `WRITE` access is held.
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

/// Borrows the contiguous `FfiEntity` slice for the current chunk.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_entities(iter: *const RawChunkIter) -> *const FfiEntity {
    if iter.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &*(iter.cast::<HostChunkIter<'_>>()) };
    if host_iter.current_entity_count == 0 {
        return core::ptr::null();
    }
    host_iter.current_entities_ptr
}

/// Queries the byte stride of column `index` in `RawChunkIter`.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_column_stride(iter: *const RawChunkIter, index: usize) -> usize {
    if iter.is_null() {
        return 0;
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &*(iter.cast::<HostChunkIter<'_>>()) };
    if index >= host_iter.query_columns.len() {
        return 0;
    }
    host_iter.query_columns[index].stride
}

/// Resets a `RawChunkIter` back to its first matched chunk for nested query iteration.
///
/// # Safety
/// `iter` must be a valid pointer to a [`HostChunkIter`] or null.
pub unsafe extern "C" fn host_iter_reset(iter: *mut RawChunkIter) -> FfiResult {
    if iter.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter.cast::<HostChunkIter<'_>>()) };
    host_iter.reset();
    FfiResult::OK
}

/// Borrows a read-only pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`HostChunkIter`] or null.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_iter_resource_readonly(
    iter: *const RawChunkIter,
    resource_name: *const c_char,
) -> *const u8 {
    if iter.is_null() || resource_name.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Caller passes a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(resource_name) }).to_str() else {
        return core::ptr::null();
    };
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &*(iter.cast::<HostChunkIter<'_>>()) };
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

/// Borrows an exclusive mutable pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`HostChunkIter`] or null.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_iter_resource_mutable(
    iter: *mut RawChunkIter,
    resource_name: *const c_char,
) -> *mut u8 {
    if iter.is_null() || resource_name.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Caller passes a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(resource_name) }).to_str() else {
        return core::ptr::null_mut();
    };
    // SAFETY: Caller passes a valid pointer to `HostChunkIter` cast to `RawChunkIter`.
    let host_iter = unsafe { &mut *(iter.cast::<HostChunkIter<'_>>()) };
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

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;

    #[test]
    fn regression_s10_entity_bits_layout() {
        let mut world = World::new();
        let mut samples = Vec::new();

        // Spawn and despawn cycles to exercise both diverse indices and bumped generations.
        for _ in 0..64 {
            samples.push(world.spawn_empty().id());
        }
        for e in samples.drain(..32).collect::<Vec<_>>() {
            world.despawn(e);
        }
        for _ in 0..48 {
            samples.push(world.spawn_empty().id());
        }
        samples.push(Entity::PLACEHOLDER);

        // Also synthesize explicit bit-patterns across boundary indices and generations.
        for &(idx, generation_bump) in &[
            (0u32, 1u32),
            (1u32, 1u32),
            (0x7FFF_FFFEu32, 1u32),
            (42u32, 0x1234_5678u32),
            (u32::MAX - 1, u32::MAX),
        ] {
            let raw_bits = ((generation_bump as u64) << 32) | (idx as u64);
            if let Some(ent) = Entity::try_from_bits(raw_bits) {
                samples.push(ent);
            }
        }

        for entity in samples {
            // SAFETY: `Entity` and `u64` (`FfiEntity`) have identical size (8) and alignment (8),
            // asserted at compile time above; `transmute_copy` verifies bit-pattern equivalence
            // with `Entity::to_bits()` relied upon by zero-copy `Table::entities()` slice casts.
            let raw_mem_bits: u64 = unsafe { core::mem::transmute_copy(&entity) };
            assert_eq!(
                raw_mem_bits,
                entity.to_bits(),
                "Entity memory representation diverged from Entity::to_bits() for {entity:?}"
            );
            let ffi = FfiEntity::from_bits(raw_mem_bits);
            assert_eq!(Entity::try_from_bits(ffi.to_bits()), Some(entity));
        }
    }
}
