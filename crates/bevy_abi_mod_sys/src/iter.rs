//! Opaque query iterator handle, column/resource introspection, and iteration FFI function types.

use core::marker::{PhantomData, PhantomPinned};

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
