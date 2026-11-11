//! Safe archetype chunk iterator wrapper tracking active chunk length internally.

use core::ffi::CStr;

use bevy_abi_mod_guest_sys::{ValidatedIter, ValidatedIterAdapter};
use bevy_abi_mod_sys::{AbiPod, FfiEntity, FfiResult};

use crate::component::GuestComponent;
use crate::iter::chunk::Chunk;
use crate::iter::slice::{ChunkSlice, ChunkSliceMut};

/// Safe wrapper around the host's chunk iterator handle.
///
/// Tracks the active chunk row count internally via [`GuestChunkIter::next_chunk`] and resets it
/// to `0` on [`GuestChunkIter::reset`] and exhaustion, preventing caller-supplied out-of-bounds
/// slice lengths.
///
/// # Compile-Fail Safety Checks
///
/// Constructing `GuestChunkIter` from a raw pointer without `unsafe` is rejected:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// fn forge_iter(api: &HostApi) {
///     let _iter = GuestChunkIter::new(core::ptr::null_mut(), api);
/// }
/// ```
///
/// Passing an arbitrary caller-supplied `count` to `entities_slice` does not compile:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::GuestChunkIter;
///
/// fn oob_entities(iter: &GuestChunkIter<'_>) {
///     let _ = iter.entities_slice(9999);
/// }
/// ```
///
/// Advancing the iterator while holding a slice borrowed from an active [`Chunk`] is rejected by
/// the borrow checker:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::GuestChunkIter;
///
/// fn use_after_advance(iter: &mut GuestChunkIter<'_>) {
///     let chunk = iter.next_chunk_view().unwrap();
///     let col = chunk.column::<f32>(0).unwrap();
///     let _next = iter.next_chunk_view();
///     let _ = col[0];
/// }
/// ```
pub struct GuestChunkIter<'a> {
    inner: ValidatedIter<'a>,
}

impl<'a> GuestChunkIter<'a> {
    #[inline]
    pub(crate) fn from_validated(inner: ValidatedIter<'a>) -> Self {
        Self { inner }
    }

    #[inline]
    pub(crate) fn as_validated(&self) -> &ValidatedIter<'a> {
        &self.inner
    }

    #[inline]
    pub(crate) fn as_validated_mut(&mut self) -> &mut ValidatedIter<'a> {
        &mut self.inner
    }

    /// Returns the number of rows in the currently active chunk (`0` before `next_chunk`, on
    /// `reset`, or after exhaustion).
    #[inline]
    pub const fn current_len(&self) -> usize {
        self.inner.current_len()
    }

    /// Rewinds the chunk iterator cursor back to the first matched chunk and zeroes `current_len`.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        self.inner.reset()
    }

    /// Advances to the next archetype table chunk, updates `current_len`, and returns the entity
    /// count (`0` when exhausted).
    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        self.inner.next_chunk()
    }

    /// Advances to the next chunk and returns a scoped [`Chunk`] view if non-empty (`None` when exhausted).
    #[inline]
    pub fn next_chunk_view(&mut self) -> Option<Chunk<'_, 'a>> {
        let count = self.next_chunk();
        if count == 0 {
            None
        } else {
            Some(Chunk::new(self))
        }
    }

    /// Obtains a read-only pointer to the start of column `index` in the current chunk.
    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        self.inner.column_readonly(index)
    }

    /// Obtains a mutable pointer to the start of column `index` in the current chunk.
    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        self.inner.column_mutable(index)
    }

    /// Explicitly marks `row` within column `index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        self.inner.mark_changed(index, row)
    }

    /// Obtains a raw pointer to the contiguous array of entity IDs in the current chunk.
    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        self.inner.entities()
    }

    /// Returns a validated slice of [`FfiEntity`] identifiers for the current chunk.
    #[inline]
    pub fn entities_slice(&self) -> &[FfiEntity] {
        self.inner.entities_slice()
    }

    /// Returns the byte stride for elements in column `index`.
    #[inline]
    pub fn column_stride(&self, index: usize) -> usize {
        self.inner.column_stride(index)
    }

    /// Returns a bounds-checked read-only [`ChunkSlice`] for column `index` in the current chunk.
    #[inline]
    pub fn readonly_slice(&self, index: usize) -> ChunkSlice<'_> {
        ChunkSlice::from_validated(self.inner.readonly_slice(index))
    }

    /// Returns a bounds-checked mutable [`ChunkSliceMut`] for column `index` in the current chunk.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize) -> ChunkSliceMut<'_> {
        ChunkSliceMut::from_validated(self.inner.mutable_slice(index))
    }

    /// Simultaneously borrows `entities`, `R` read-only columns, and `W` disjoint mutable columns
    /// from the active chunk.
    #[allow(clippy::type_complexity)]
    #[inline]
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        let (entities, ro, rw) = self.inner.split_slices(read_cols, write_cols)?;
        Some((
            entities,
            ro.map(ChunkSlice::from_validated),
            rw.map(ChunkSliceMut::from_validated),
        ))
    }

    /// Borrows `N` disjoint mutable columns simultaneously from the active chunk.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        let rw = self.inner.columns_mut(indices)?;
        Some(rw.map(ChunkSliceMut::from_validated))
    }

    /// Simultaneously borrows one typed read-only column (`&[R]`) and one typed mutable column (`&mut [W]`).
    #[inline]
    pub fn read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        self.inner.read_write::<R, W>(read_col, write_col)
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: AbiPod, W2: AbiPod>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        self.inner.write_2::<W1, W2>(col_a, col_b)
    }

    /// Obtains an optional read-only column pointer for the current chunk.
    #[inline]
    pub fn optional_column_ptr(&self, index: usize) -> Option<*const u8> {
        self.inner.optional_column_ptr(index)
    }

    /// Obtains an optional mutable column pointer for the current chunk.
    #[inline]
    pub fn optional_column_mut_ptr(&mut self, index: usize) -> Option<*mut u8> {
        self.inner.optional_column_mut_ptr(index)
    }

    /// Returns `Some(ChunkSlice)` when an `OPTIONAL_READ` column is present on the current chunk.
    #[inline]
    pub fn optional_readonly_slice(&self, index: usize) -> Option<ChunkSlice<'_>> {
        self.inner
            .optional_readonly_slice(index)
            .map(ChunkSlice::from_validated)
    }

    /// Returns `Some(ChunkSliceMut)` when an `OPTIONAL_WRITE` column is present on the current chunk.
    #[inline]
    pub fn optional_mutable_slice(&mut self, index: usize) -> Option<ChunkSliceMut<'_>> {
        self.inner
            .optional_mutable_slice(index)
            .map(ChunkSliceMut::from_validated)
    }

    /// Simultaneously borrows an optional read-only column (`Option<&[OptR]>`) and a required
    /// disjoint mutable column (`&mut [W]`).
    #[inline]
    pub fn optional_read_write<OptR: AbiPod, W: AbiPod>(
        &mut self,
        opt_read_col: usize,
        write_col: usize,
    ) -> Option<(Option<&[OptR]>, &mut [W])> {
        self.inner
            .optional_read_write::<OptR, W>(opt_read_col, write_col)
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        self.inner.resource_ptr(resource_name)
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`).
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        self.inner.resource_mut_ptr(resource_name)
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`Res<R>`) for reading.
    #[inline]
    pub fn resource<R: AbiPod>(&self, resource_name: &CStr) -> Option<&R> {
        self.inner.resource::<R>(resource_name)
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`ResMut<R>`) mutably.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        self.inner.resource_mut::<R>(resource_name)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`Res<R>`) using `R::NAME`.
    #[inline]
    pub fn guest_resource<R: GuestComponent>(&self) -> Option<&R> {
        self.resource::<R>(R::NAME)
    }

    /// Safely borrows a typed [`GuestComponent`] resource (`ResMut<R>`) mutably using `R::NAME`.
    #[inline]
    pub fn guest_resource_mut<R: GuestComponent>(&mut self) -> Option<&mut R> {
        self.resource_mut::<R>(R::NAME)
    }
}

impl<'a> ValidatedIterAdapter<'a> for GuestChunkIter<'a> {
    #[inline]
    fn from_validated_iter(inner: ValidatedIter<'a>) -> Self {
        Self { inner }
    }

    #[inline]
    fn as_validated_iter_mut(&mut self) -> &mut ValidatedIter<'a> {
        &mut self.inner
    }
}
