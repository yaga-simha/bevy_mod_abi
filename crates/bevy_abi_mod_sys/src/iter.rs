//! Opaque query iterator handle, column/resource introspection, and iteration FFI function types.

use core::{
    ffi::c_void,
    marker::{PhantomData, PhantomPinned},
};

use crate::{
    access::AccessMode,
    primitives::{BamComponent, ComponentUuid, FfiBool, FfiEntity, FfiResult},
};

/// Opaque C handle representing an active query chunk and singleton resource accessor (`typedef struct BamIter BamIter;`).
#[repr(C)]
pub struct BamIter {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Column or component uses `StorageType::SparseSet` rather than dense `Table` storage.
pub const BAM_STORAGE_FLAG_SPARSE_SET: u32 = 1 << 0;

/// Column or component is registered as immutable (mutable access is rejected).
pub const BAM_STORAGE_FLAG_IMMUTABLE: u32 = 1 << 1;

/// Term refers to a singleton Bevy `Resource` rather than a per-entity component column.
pub const BAM_STORAGE_FLAG_RESOURCE: u32 = 1 << 2;

/// Term refers to a buffered Bevy `Messages<M>` stream (`F-02`).
pub const BAM_STORAGE_FLAG_MESSAGE: u32 = 1 << 3;

/// Registered type is a triggerable Bevy `Event` (`world.trigger` / dynamic observer).
pub const BAM_STORAGE_FLAG_EVENT: u32 = 1 << 4;

/// Registered type is an entity-targeted Bevy `EntityEvent`.
pub const BAM_STORAGE_FLAG_ENTITY_EVENT: u32 = 1 << 5;

/// Maximum change-tick relative age matching `bevy_ecs::change_detection::MAX_CHANGE_AGE`.
pub const BAM_MAX_CHANGE_AGE: u32 = u32::MAX - (2 * 518_400_000 - 1);

/// Snapshot of Bevy `ComponentTicks` (`added`, `changed`) alongside the enclosing system's
/// `(last_run, this_run)` tick window (`typedef struct BamChangeTicks BamChangeTicks;`).
///
/// Layout: `16` bytes, `4`-byte alignment, zero implicit padding on 32-bit and 64-bit targets.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq, Hash)]
pub struct BamChangeTicks {
    /// Tick when the resource or component was added (`offset 0`).
    pub added: u32,
    /// Tick when the resource or component was last changed (`offset 4`).
    pub changed: u32,
    /// Calling system's `last_run` tick (`offset 8`).
    pub last_run: u32,
    /// Calling system's `this_run` tick (`offset 12`).
    pub this_run: u32,
}

impl BamChangeTicks {
    /// Evaluates wraparound-safe Bevy change detection: returns `true` iff `tick` occurred strictly
    /// after `last_run` relative to `this_run`, clamping ages at [`BAM_MAX_CHANGE_AGE`].
    #[inline]
    pub const fn is_tick_newer(tick: u32, last_run: u32, this_run: u32) -> bool {
        let ticks_since_insert = this_run.wrapping_sub(tick);
        let clamped_insert = if ticks_since_insert < BAM_MAX_CHANGE_AGE {
            ticks_since_insert
        } else {
            BAM_MAX_CHANGE_AGE
        };
        let ticks_since_system = this_run.wrapping_sub(last_run);
        let clamped_system = if ticks_since_system < BAM_MAX_CHANGE_AGE {
            ticks_since_system
        } else {
            BAM_MAX_CHANGE_AGE
        };
        clamped_system > clamped_insert
    }

    /// Returns `true` if the resource or component was added since the system's `last_run` tick.
    #[inline]
    pub const fn is_added(&self) -> bool {
        Self::is_tick_newer(self.added, self.last_run, self.this_run)
    }

    /// Returns `true` if the resource or component was changed since the system's `last_run` tick.
    #[inline]
    pub const fn is_changed(&self) -> bool {
        Self::is_tick_newer(self.changed, self.last_run, self.this_run)
    }
}

const _: () = {
    assert!(core::mem::size_of::<BamChangeTicks>() == 16);
    assert!(core::mem::align_of::<BamChangeTicks>() == 4);
    assert!(core::mem::offset_of!(BamChangeTicks, added) == 0);
    assert!(core::mem::offset_of!(BamChangeTicks, changed) == 4);
    assert!(core::mem::offset_of!(BamChangeTicks, last_run) == 8);
    assert!(core::mem::offset_of!(BamChangeTicks, this_run) == 12);
};

/// Metadata returned by [`BamIterColumnInfoFn`] (`iter_column_info`) for guest type-confusion checks.
///
/// Layout:
/// - 64-bit: `48` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `40` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BamColumnInfo {
    /// Resolved 1-based [`BamComponent`] handle for this term (`offset 0`).
    pub component: BamComponent,
    /// Declared [`AccessMode`] for this term (`offset 4`).
    pub mode: AccessMode,
    /// Registered 128-bit [`ComponentUuid`] (`offset 8`, 16 bytes).
    pub uuid: ComponentUuid,
    /// Element byte size (`size_of::<T>()`, `offset 24`).
    pub size: usize,
    /// Element byte alignment (`align_of::<T>()`, `offset 32` on 64-bit / `28` on 32-bit).
    pub align: usize,
    /// Bitmask of `BAM_STORAGE_FLAG_*` (`offset 40` on 64-bit / `32` on 32-bit).
    pub storage_flags: u32,
    /// Reserved padding word (`0`, `offset 44` on 64-bit / `36` on 32-bit).
    pub reserved0: u32,
}

impl BamColumnInfo {
    /// Returns `true` if `BAM_STORAGE_FLAG_SPARSE_SET` is set.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_IMMUTABLE` is set.
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_IMMUTABLE) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_RESOURCE` is set.
    #[inline]
    pub const fn is_resource(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_RESOURCE) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_MESSAGE` is set (`F-02`).
    #[inline]
    pub const fn is_message(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_MESSAGE) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_EVENT` is set.
    #[inline]
    pub const fn is_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_EVENT) != 0
    }

    /// Returns `true` if `BAM_STORAGE_FLAG_ENTITY_EVENT` is set.
    #[inline]
    pub const fn is_entity_event(&self) -> bool {
        (self.storage_flags & BAM_STORAGE_FLAG_ENTITY_EVENT) != 0
    }

    /// Returns `true` if `(uuid, size, align)` match the expected guest type metadata.
    #[inline]
    pub fn matches_layout(&self, expected_uuid: ComponentUuid, size: usize, align: usize) -> bool {
        self.uuid == expected_uuid && self.size == size && self.align == align
    }
}

const _: () = {
    #[cfg(target_pointer_width = "64")]
    {
        assert!(core::mem::size_of::<BamColumnInfo>() == 48);
        assert!(core::mem::align_of::<BamColumnInfo>() == 8);
        assert!(core::mem::offset_of!(BamColumnInfo, component) == 0);
        assert!(core::mem::offset_of!(BamColumnInfo, mode) == 4);
        assert!(core::mem::offset_of!(BamColumnInfo, uuid) == 8);
        assert!(core::mem::offset_of!(BamColumnInfo, size) == 24);
        assert!(core::mem::offset_of!(BamColumnInfo, align) == 32);
        assert!(core::mem::offset_of!(BamColumnInfo, storage_flags) == 40);
        assert!(core::mem::offset_of!(BamColumnInfo, reserved0) == 44);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(core::mem::size_of::<BamColumnInfo>() == 40);
        assert!(core::mem::align_of::<BamColumnInfo>() == 4);
        assert!(core::mem::offset_of!(BamColumnInfo, component) == 0);
        assert!(core::mem::offset_of!(BamColumnInfo, mode) == 4);
        assert!(core::mem::offset_of!(BamColumnInfo, uuid) == 8);
        assert!(core::mem::offset_of!(BamColumnInfo, size) == 24);
        assert!(core::mem::offset_of!(BamColumnInfo, align) == 28);
        assert!(core::mem::offset_of!(BamColumnInfo, storage_flags) == 32);
        assert!(core::mem::offset_of!(BamColumnInfo, reserved0) == 36);
    }
};

/// Advances `iter` to the next non-empty chunk, writing the chunk length into `*out_len` and
/// returning [`FfiBool::TRUE`] while chunks remain or [`FfiBool::FALSE`] when exhausted.
///
/// # Safety
/// - `iter` must point to an active [`BamIter`] belonging to the running system invocation.
/// - `out_len` must be a valid writable pointer to a `usize` (or null, which returns [`FfiBool::FALSE`]).
pub type BamIterNextChunkFn =
    unsafe extern "C" fn(iter: *mut BamIter, out_len: *mut usize) -> FfiBool;

/// Resets `iter` back to its initial chunk so nested query loops can re-traverse without allocation.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] belonging to the running system invocation.
pub type BamIterResetFn = unsafe extern "C" fn(iter: *mut BamIter) -> FfiResult;

/// Returns a pointer to the contiguous `[FfiEntity; chunk_len]` slice for the current chunk, or null.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk.
pub type BamIterEntitiesFn = unsafe extern "C" fn(iter: *const BamIter) -> *const FfiEntity;

/// Returns a read-only byte pointer to dense column `term_index` in the current chunk, or null
/// if out of bounds, absent on an optional term, sparse-set stored, or non-column filter/resource.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk.
pub type BamIterColumnReadonlyFn =
    unsafe extern "C" fn(iter: *const BamIter, term_index: usize) -> *const u8;

/// Returns an exclusive mutable byte pointer to dense column `term_index` in the current chunk,
/// marking all rows in the chunk as changed at most once per chunk.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk, and the caller must
/// not create aliasing references to the same column.
pub type BamIterColumnMutableFn =
    unsafe extern "C" fn(iter: *mut BamIter, term_index: usize) -> *mut u8;

/// Returns an exclusive mutable byte pointer to dense column `term_index` in the current chunk
/// without automatically marking change ticks, for pairing with [`BamIterMarkChangedRangeFn`].
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk, and the caller must
/// not create aliasing references to the same column.
pub type BamIterColumnMutableUntrackedFn =
    unsafe extern "C" fn(iter: *mut BamIter, term_index: usize) -> *mut u8;

/// Marks rows `start_row..start_row + row_count` in writable column `term_index` as changed.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk.
pub type BamIterMarkChangedRangeFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    term_index: usize,
    start_row: usize,
    row_count: usize,
) -> FfiResult;

/// Materializes and returns a pointer to `[ *const u8; chunk_len ]` of per-row read-only component
/// pointers for `StorageType::SparseSet` columns.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk.
pub type BamIterColumnRowsReadonlyFn =
    unsafe extern "C" fn(iter: *mut BamIter, term_index: usize) -> *const *const u8;

/// Materializes and returns a pointer to `[ *mut u8; chunk_len ]` of per-row mutable component
/// pointers for `StorageType::SparseSet` columns, marking matched rows changed.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] positioned at a valid chunk, and the caller must
/// not create aliasing mutable borrows across rows.
pub type BamIterColumnRowsMutableFn =
    unsafe extern "C" fn(iter: *mut BamIter, term_index: usize) -> *const *mut u8;

/// Writes type, layout, and storage metadata for `term_index` (entity column or resource term)
/// into `*out_info`.
///
/// # Safety
/// - `iter` must point to an active [`BamIter`].
/// - `out_info` must point to writable [`BamColumnInfo`] storage.
pub type BamIterColumnInfoFn = unsafe extern "C" fn(
    iter: *const BamIter,
    term_index: usize,
    out_info: *mut BamColumnInfo,
) -> FfiResult;

/// Returns a read-only pointer to the singleton `Resource` declared at `term_index`, or null if
/// `term_index` is not a `RESOURCE_READ`/`RESOURCE_WRITE` term or the resource is absent in the `World`.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] belonging to the running system invocation.
pub type BamResPtrFn = unsafe extern "C" fn(iter: *const BamIter, term_index: usize) -> *const u8;

/// Returns an exclusive mutable pointer to the singleton `Resource` declared with `RESOURCE_WRITE`
/// at `term_index` (marking its Bevy change tick), or null if unauthorized or absent.
///
/// # Safety
/// `iter` must point to an active [`BamIter`] belonging to the running system invocation, and
/// the caller must not create aliasing references to the resource.
pub type BamResMutPtrFn = unsafe extern "C" fn(iter: *mut BamIter, term_index: usize) -> *mut u8;

/// Task callback invoked on worker threads by [`BamParForEachChunkFn`] (`P-07`).
///
/// Each task receives an isolated sub-range [`BamIter`] whose `iter_next_chunk` yields only the
/// disjoint chunks assigned to that task.
///
/// # Safety
/// - `iter` is valid strictly for the duration of the task callback invocation on that worker thread.
/// - The callback must be thread-safe (`Sync` over shared `user_data`) and must never unwind across `extern "C"`.
pub type BamChunkTaskFn =
    unsafe extern "C" fn(iter: *mut BamIter, user_data: *mut c_void) -> FfiResult;

/// Partitions matched tables/archetype runs of `iter` across the host `ComputeTaskPool`, invoking
/// `chunk_fn(sub_iter, user_data)` on disjoint sub-range iterators in parallel (`P-07`).
///
/// # Safety
/// - `iter` must point to an active top-level [`BamIter`] belonging to the running system invocation.
/// - `chunk_fn` and `user_data` must be safe to invoke concurrently across worker threads.
pub type BamParForEachChunkFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult;

/// Reads up to `max_count` unread POD messages from the Bevy `Messages<M>` stream declared at
/// `term_index` (`MESSAGE_READ` or `MESSAGE_WRITE`) into `out_buf` (`max_count * size_of::<M>()`
/// writable bytes), advancing the running system's per-term message cursor and writing the count
/// of messages copied into `*out_read` (`F-02`).
///
/// # Safety
/// - `iter` must point to an active [`BamIter`] belonging to the running system invocation.
/// - `out_read` must point to writable `usize` storage.
/// - When `max_count > 0` and element size `> 0`, `out_buf` must point to at least
///   `max_count * elem_size` writable bytes.
pub type BamMsgReadBatchFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    term_index: usize,
    out_buf: *mut u8,
    max_count: usize,
    out_read: *mut usize,
) -> FfiResult;

/// Writes one POD message payload (`data_ptr[0..data_len]`) into the Bevy `Messages<M>` stream
/// declared with `MESSAGE_WRITE` at `term_index` (`F-02`).
///
/// # Safety
/// - `iter` must point to an active [`BamIter`] belonging to the running system invocation.
/// - When `data_len > 0`, `data_ptr` must point to `data_len` readable bytes representing a valid `M`.
pub type BamMsgSendFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    term_index: usize,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Writes `count` contiguous POD message payloads from `in_buf` (`count * size_of::<M>()` readable
/// bytes) into the Bevy `Messages<M>` stream declared with `MESSAGE_WRITE` at `term_index`.
///
/// # Safety
/// - `iter` must point to an active [`BamIter`] belonging to the running system invocation.
/// - When `count > 0` and element size `> 0`, `in_buf` must point to `count * elem_size` readable bytes.
pub type BamMsgWriteBatchFn = unsafe extern "C" fn(
    iter: *mut BamIter,
    term_index: usize,
    in_buf: *const u8,
    count: usize,
) -> FfiResult;

/// Queries Bevy change-detection ticks (`added`, `changed`, `last_run`, `this_run`) for the
/// singleton `Resource` declared at `term_index` (`RESOURCE_READ` or `RESOURCE_WRITE`), writing
/// them into `*out_ticks`.
///
/// # Safety
/// - `iter` must point to an active [`BamIter`] belonging to the running system invocation.
/// - `out_ticks` must point to writable [`BamChangeTicks`] storage.
pub type BamResTicksFn = unsafe extern "C" fn(
    iter: *const BamIter,
    term_index: usize,
    out_ticks: *mut BamChangeTicks,
) -> FfiResult;
