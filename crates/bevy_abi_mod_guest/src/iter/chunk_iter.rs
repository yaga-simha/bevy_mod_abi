//! Safe query chunk iterator (`ChunkIter`) and singleton resource accessor.

use core::marker::PhantomData;

use bevy_abi_mod_guest_sys::{Host, SparseRows, SparseRowsMut, ValidatedIter};
use bevy_abi_mod_sys::{AbiPod, BamChangeTicks, BamColumnInfo, BamIter, FfiEntity, FfiResult};

use crate::{
    app::{GuestRes, GuestResMut, IntoSystemResult},
    component::{GuestComponent, guest_uuid_filter},
    iter::{
        chunk::Chunk,
        slice::{ChunkSlice, ChunkSliceMut},
        typed_query::{QueryTuple, TypedChunkIter},
    },
};

/// Safe query chunk iterator wrapping an active [`BamIter`] handle.
///
/// Constructing a `ChunkIter` directly from raw C pointers in safe Rust is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkIter;
///
/// let _iter = ChunkIter::from_raw(core::ptr::null_mut());
/// ```
///
/// Advancing the iterator while a [`Chunk`] or column slice borrow is still live is rejected by
/// Rust's borrow checker:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkIter;
///
/// fn use_after_advance(iter: &mut ChunkIter<'_>) {
///     let chunk = iter.next_chunk_view().unwrap();
///     let col = chunk.column::<f32>(0).unwrap();
///     let _ = iter.next_chunk_view();
///     let _ = col[0];
/// }
/// ```
pub struct ChunkIter<'a> {
    pub(crate) inner: ValidatedIter<'a>,
}

impl<'a> ChunkIter<'a> {
    /// Wraps an already-validated [`ValidatedIter`].
    #[inline]
    pub const fn from_validated(inner: ValidatedIter<'a>) -> Self {
        Self { inner }
    }

    /// Returns `true` if this iterator wraps a non-null [`BamIter`] handle.
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }

    /// Returns the raw `*mut BamIter` pointer for FFI interoperability.
    #[inline]
    pub const fn as_raw_ptr(&self) -> *mut BamIter {
        self.inner.as_raw()
    }

    /// Returns the cached [`Host`] table for this mod instance.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.inner.host()
    }

    /// Returns the number of entities in the currently active chunk (`0` before `next_chunk` or
    /// after exhaustion).
    #[inline]
    pub const fn current_len(&self) -> usize {
        self.inner.current_len()
    }

    /// Advances to the next non-empty chunk, returning `Some(chunk_len)` while chunks remain or
    /// `None` when exhausted.
    #[inline]
    pub fn next_chunk(&mut self) -> Option<usize> {
        self.inner.next_chunk()
    }

    /// Advances to the next non-empty chunk and returns a borrow-checked [`Chunk`] view.
    #[inline]
    pub fn next_chunk_view(&mut self) -> Option<Chunk<'_, 'a>> {
        let len = self.inner.next_chunk()?;
        Some(Chunk {
            iter: &mut self.inner,
            len,
        })
    }

    /// Resets the iterator back to its first chunk so nested query loops can re-traverse without
    /// allocation.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        self.inner.reset()
    }

    /// Queries [`BamColumnInfo`] metadata for `term_index`.
    #[inline]
    pub fn column_info(&self, term_index: usize) -> Result<BamColumnInfo, FfiResult> {
        self.inner.column_info(term_index)
    }

    /// Borrows the `[FfiEntity; current_len]` slice for the active chunk.
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        self.inner.entities()
    }

    /// Returns a low-level [`ChunkSlice`] view over dense read column `col`.
    #[inline]
    pub fn readonly_slice(&self, col: usize) -> ChunkSlice<'_> {
        ChunkSlice::from_validated(self.inner.readonly_slice(col))
    }

    /// Returns a low-level [`ChunkSliceMut`] view over dense write column `col` (marking ticks changed).
    #[inline]
    pub fn mutable_slice(&mut self, col: usize) -> ChunkSliceMut<'_> {
        ChunkSliceMut::from_validated(self.inner.mutable_slice(col))
    }

    /// Returns an untracked [`ChunkSliceMut`] view over dense write column `col`.
    #[inline]
    pub fn mutable_slice_untracked(&mut self, col: usize) -> ChunkSliceMut<'_> {
        ChunkSliceMut::from_validated(self.inner.mutable_slice_untracked(col))
    }

    /// Marks rows `start_row..start_row + row_count` in writable column `col` as changed.
    #[inline]
    pub fn mark_changed_range(
        &mut self,
        col: usize,
        start_row: usize,
        row_count: usize,
    ) -> FfiResult {
        self.inner.mark_changed_range(col, start_row, row_count)
    }

    /// Marks single row `row` in writable column `col` as changed.
    #[inline]
    pub fn mark_changed(&mut self, col: usize, row: usize) -> FfiResult {
        self.inner.mark_changed(col, row)
    }

    /// Materializes indirect read-only sparse-set row pointers `[ *const u8; current_len ]`.
    #[inline]
    pub fn rows<T: AbiPod>(&mut self, col: usize) -> Option<SparseRows<'_, T>> {
        self.inner.rows_checked::<T>(col, None)
    }

    /// Materializes indirect read-only sparse-set row pointers for `T: GuestComponent`.
    #[inline]
    pub fn component_rows<T: GuestComponent>(&mut self, col: usize) -> Option<SparseRows<'_, T>> {
        let filter = guest_uuid_filter::<T>(self.inner.host().manifest_name());
        self.inner.rows_checked::<T>(col, Some(filter))
    }

    /// Materializes indirect mutable sparse-set row pointers `[ *mut u8; current_len ]`.
    #[inline]
    pub fn rows_mut<T: AbiPod>(&mut self, col: usize) -> Option<SparseRowsMut<'_, T>> {
        self.inner.rows_mut_checked::<T>(col, None)
    }

    /// Materializes indirect mutable sparse-set row pointers for `T: GuestComponent`.
    #[inline]
    pub fn component_rows_mut<T: GuestComponent>(
        &mut self,
        col: usize,
    ) -> Option<SparseRowsMut<'_, T>> {
        let filter = guest_uuid_filter::<T>(self.inner.host().manifest_name());
        self.inner.rows_mut_checked::<T>(col, Some(filter))
    }

    /// Borrows singleton resource `R` (`R: AbiPod`) declared at `term_index` in this query.
    #[inline]
    pub fn resource<R: AbiPod>(&self, term_index: usize) -> Option<&R> {
        self.inner.resource_checked::<R>(term_index, None)
    }

    /// Borrows singleton resource `R` (`R: GuestComponent`) declared at `term_index`, validating
    /// `R`'s UUID.
    #[inline]
    pub fn component_resource<R: GuestComponent>(&self, term_index: usize) -> Option<&R> {
        let filter = guest_uuid_filter::<R>(self.inner.host().manifest_name());
        self.inner.resource_checked::<R>(term_index, Some(filter))
    }

    /// Mutably borrows singleton resource `R` (`R: AbiPod`) declared with `RESOURCE_WRITE` at
    /// `term_index`.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, term_index: usize) -> Option<&mut R> {
        self.inner.resource_mut_checked::<R>(term_index, None)
    }

    /// Mutably borrows singleton resource `R` (`R: GuestComponent`) declared at `term_index`,
    /// validating `R`'s UUID.
    #[inline]
    pub fn component_resource_mut<R: GuestComponent>(
        &mut self,
        term_index: usize,
    ) -> Option<&mut R> {
        let filter = guest_uuid_filter::<R>(self.inner.host().manifest_name());
        self.inner
            .resource_mut_checked::<R>(term_index, Some(filter))
    }

    /// Reads Bevy change-detection ticks for the singleton resource declared at `term_index`.
    #[inline]
    pub fn resource_ticks(&self, term_index: usize) -> Result<BamChangeTicks, FfiResult> {
        self.inner.res_ticks(term_index)
    }

    /// Borrows singleton resource `R` (`R: GuestComponent`) wrapped with its [`BamChangeTicks`].
    #[inline]
    pub fn component_resource_with_ticks<R: GuestComponent>(
        &self,
        term_index: usize,
    ) -> Result<GuestRes<'_, R>, FfiResult> {
        let filter = guest_uuid_filter::<R>(self.inner.host().manifest_name());
        self.inner
            .resource_with_ticks_checked::<R>(term_index, Some(filter))
            .map(|(val, ticks)| GuestRes::new(val, ticks))
            .ok_or(FfiResult::NOT_FOUND)
    }

    /// Mutably borrows singleton resource `R` (`R: GuestComponent`) wrapped with its prior
    /// [`BamChangeTicks`].
    #[inline]
    pub fn component_resource_mut_with_ticks<R: GuestComponent>(
        &mut self,
        term_index: usize,
    ) -> Result<GuestResMut<'_, R>, FfiResult> {
        let filter = guest_uuid_filter::<R>(self.inner.host().manifest_name());
        self.inner
            .resource_mut_with_ticks_checked::<R>(term_index, Some(filter))
            .map(|(val, ticks)| GuestResMut::new(val, ticks))
            .ok_or(FfiResult::NOT_FOUND)
    }

    /// Processes disjoint chunk partitions in parallel on the host `ComputeTaskPool`.
    ///
    /// Panics inside `f` are caught per worker task and surfaced as [`FfiResult::PANIC_CAUGHT`]
    /// without unwinding across `extern "C"`.
    #[inline]
    pub fn par_for_each<F>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Chunk<'_, '_>) + Sync,
    {
        self.inner.par_for_each_chunk(|task_iter| {
            let len = task_iter.current_len();
            f(Chunk {
                iter: task_iter,
                len,
            });
            FfiResult::OK
        })
    }

    /// Fallible variant of [`Self::par_for_each`] propagating non-`OK` [`IntoSystemResult`] returns.
    #[inline]
    pub fn par_try_for_each<F, R>(&mut self, f: F) -> FfiResult
    where
        F: Fn(Chunk<'_, '_>) -> R + Sync,
        R: IntoSystemResult,
    {
        self.inner.par_for_each_chunk(|task_iter| {
            let len = task_iter.current_len();
            f(Chunk {
                iter: task_iter,
                len,
            })
            .into_ffi_result()
        })
    }

    /// Wraps this iterator in a [`TypedChunkIter`] for [`QueryTuple`] `Q`.
    #[inline]
    pub fn typed<Q: QueryTuple>(&mut self) -> TypedChunkIter<'_, 'a, Q> {
        TypedChunkIter {
            iter: self,
            _marker: PhantomData,
        }
    }

    /// Drains unread POD messages (`M: AbiPod`) from `term_index`.
    #[inline]
    pub fn read_pod_messages<M: AbiPod>(&mut self, term_index: usize) -> Result<Vec<M>, FfiResult> {
        self.inner.drain_messages_checked::<M>(term_index, None)
    }

    /// Drains unread messages (`M: GuestComponent`) from `term_index`, validating `M`'s UUID.
    #[inline]
    pub fn read_messages<M: GuestComponent>(
        &mut self,
        term_index: usize,
    ) -> Result<Vec<M>, FfiResult> {
        let filter = guest_uuid_filter::<M>(self.inner.host().manifest_name());
        self.inner
            .drain_messages_checked::<M>(term_index, Some(filter))
    }

    /// Sends a POD message (`M: AbiPod`) on `term_index` declared with `MESSAGE_WRITE`.
    #[inline]
    pub fn send_pod_message<M: AbiPod>(&mut self, term_index: usize, msg: &M) -> FfiResult {
        self.inner.send_message_checked::<M>(term_index, None, msg)
    }

    /// Sends a message (`M: GuestComponent`) on `term_index`, validating `M`'s UUID.
    #[inline]
    pub fn send_message<M: GuestComponent>(&mut self, term_index: usize, msg: &M) -> FfiResult {
        let filter = guest_uuid_filter::<M>(self.inner.host().manifest_name());
        self.inner
            .send_message_checked::<M>(term_index, Some(filter), msg)
    }

    /// Batch-sends POD messages (`M: AbiPod`) on `term_index` declared with `MESSAGE_WRITE`.
    #[inline]
    pub fn send_pod_messages<M: AbiPod>(
        &mut self,
        term_index: usize,
        messages: &[M],
    ) -> Result<(), FfiResult> {
        let status = self
            .inner
            .send_messages_batch_checked::<M>(term_index, None, messages);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }

    /// Batch-sends messages (`M: GuestComponent`) on `term_index`, validating `M`'s UUID.
    #[inline]
    pub fn send_messages<M: GuestComponent>(
        &mut self,
        term_index: usize,
        messages: &[M],
    ) -> Result<(), FfiResult> {
        let filter = guest_uuid_filter::<M>(self.inner.host().manifest_name());
        let status =
            self.inner
                .send_messages_batch_checked::<M>(term_index, Some(filter), messages);
        if status.is_ok() { Ok(()) } else { Err(status) }
    }
}

impl<'a> From<ValidatedIter<'a>> for ChunkIter<'a> {
    #[inline]
    fn from(inner: ValidatedIter<'a>) -> Self {
        Self::from_validated(inner)
    }
}
