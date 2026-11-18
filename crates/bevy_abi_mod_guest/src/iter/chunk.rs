//! Safe per-chunk view (`Chunk`) enforcing Rust borrow rules across dense columns, sparse rows,
//! and fine-grained change-tick marking.

use bevy_abi_mod_guest_sys::{
    ChunkColumnDispenser, SparseRows, SparseRowsMut, UuidFilter, ValidatedIter,
};
use bevy_abi_mod_sys::{AbiPod, BamColumnInfo, FfiEntity, FfiResult, ModComponent};

use crate::{
    component::{GuestComponent, guest_uuid_filter},
    iter::{
        slice::{ChunkSlice, ChunkSliceMut},
        typed_query::QueryTuple,
    },
};

/// Active archetype chunk yielded by [`ChunkIter::next_chunk_view`](crate::ChunkIter::next_chunk_view).
///
/// Enforces Rust's aliasing rules at compile time:
/// - Multiple shared read columns (`column`, `component`, `entities`) may be borrowed simultaneously
///   via `&self`.
/// - Mutable column or sparse-row views (`column_mut`, `component_mut`, `column_mut_untracked`,
///   `rows_mut`) require `&mut self`, preventing overlapping borrows unless explicitly split via
///   [`Self::read_write`] / [`Self::write_2`] (which verify distinct columns, distinct component
///   handles, and disjoint byte ranges).
///
/// ```compile_fail
/// use bevy_abi_mod_guest::Chunk;
///
/// fn simultaneous_mut(mut chunk: Chunk<'_, '_>) {
///     let a = chunk.column_mut::<f32>(0).unwrap();
///     let b = chunk.column_mut::<f32>(0).unwrap();
///     a[0] += b[0];
/// }
/// ```
///
/// Non-`AbiPod` types (`bool`, `String`, `usize`, `u128`) are rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::Chunk;
///
/// fn non_pod(chunk: &Chunk<'_, '_>) {
///     let _ = chunk.column::<bool>(0);
/// }
/// ```
pub struct Chunk<'iter, 'ctx> {
    pub(crate) iter: &'iter mut ValidatedIter<'ctx>,
    pub(crate) len: usize,
}

impl<'iter, 'ctx> Chunk<'iter, 'ctx> {
    /// Returns the number of entity rows in this chunk.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if `self.len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Queries [`BamColumnInfo`] metadata (UUID, size, align, access mode, storage flags) for
    /// declared query term `col`.
    #[inline]
    pub fn column_info(&self, col: usize) -> Result<BamColumnInfo, FfiResult> {
        self.iter.column_info(col)
    }

    /// Borrows the contiguous `[FfiEntity; len]` slice for this chunk.
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        self.iter.entities()
    }

    /// Borrows dense column `col` as `&[T]` (`T: AbiPod`), validating `(size, align, mode, storage)`
    /// via [`BamColumnInfo`]. Returns `None` on layout/mode mismatch or absent optional column.
    #[inline]
    pub fn column<T: AbiPod>(&self, col: usize) -> Option<&[T]> {
        self.iter.column_checked::<T>(col, None)
    }

    /// Borrows dense column `col` as `&[T]` (`T: GuestComponent`), validating `T`'s UUID (including
    /// manifest-qualified names) in addition to `(size, align, mode, storage)`.
    /// Returns `None` on UUID or layout mismatch.
    #[inline]
    pub fn component<T: GuestComponent>(&self, col: usize) -> Option<&[T]> {
        let filter = guest_uuid_filter::<T>(self.iter.host().manifest_name());
        self.iter.column_checked::<T>(col, Some(filter))
    }

    /// Alias for [`Self::column`] on optional read columns (`Option<&[T]>` is `None` when the
    /// archetype lacks the optional component).
    #[inline]
    pub fn optional_column<T: AbiPod>(&self, col: usize) -> Option<&[T]> {
        self.column::<T>(col)
    }

    /// Alias for [`Self::component`] on optional read columns, additionally validating `T::UUID`.
    #[inline]
    pub fn optional_component<T: GuestComponent>(&self, col: usize) -> Option<&[T]> {
        self.component::<T>(col)
    }

    /// Mutably borrows dense column `col` as `&mut [T]` (`T: AbiPod`), marking all chunk rows
    /// changed once and validating `(size, align, mode, storage)` via [`BamColumnInfo`].
    #[inline]
    pub fn column_mut<T: AbiPod>(&mut self, col: usize) -> Option<&mut [T]> {
        self.iter.column_mut_checked::<T>(col, None)
    }

    /// Mutably borrows dense column `col` as `&mut [T]` (`T: GuestComponent`), validating `T`'s
    /// UUID in addition to `(size, align, mode, storage)` and marking chunk rows changed.
    #[inline]
    pub fn component_mut<T: GuestComponent>(&mut self, col: usize) -> Option<&mut [T]> {
        let filter = guest_uuid_filter::<T>(self.iter.host().manifest_name());
        self.iter.column_mut_checked::<T>(col, Some(filter))
    }

    /// Alias for [`Self::column_mut`] on optional write columns (`None` if absent on the archetype).
    #[inline]
    pub fn optional_column_mut<T: AbiPod>(&mut self, col: usize) -> Option<&mut [T]> {
        self.column_mut::<T>(col)
    }

    /// Alias for [`Self::component_mut`] on optional write columns, additionally validating `T::UUID`.
    #[inline]
    pub fn optional_component_mut<T: GuestComponent>(&mut self, col: usize) -> Option<&mut [T]> {
        self.component_mut::<T>(col)
    }

    /// Mutably borrows dense column `col` as `&mut [T]` (`T: AbiPod`) **without** automatically
    /// marking Bevy change ticks. Pair with [`Self::mark_changed_range`] or
    /// [`Self::mark_changed`] after selectively modifying rows.
    #[inline]
    pub fn column_mut_untracked<T: AbiPod>(&mut self, col: usize) -> Option<&mut [T]> {
        self.iter.column_mut_untracked_checked::<T>(col, None)
    }

    /// Untracked mutable column slice for `T: GuestComponent`, validating `T`'s UUID.
    #[inline]
    pub fn component_mut_untracked<T: GuestComponent>(&mut self, col: usize) -> Option<&mut [T]> {
        let filter = guest_uuid_filter::<T>(self.iter.host().manifest_name());
        self.iter
            .column_mut_untracked_checked::<T>(col, Some(filter))
    }

    /// Marks rows `start_row..start_row + row_count` on writable column `col` as changed.
    #[inline]
    pub fn mark_changed_range(
        &mut self,
        col: usize,
        start_row: usize,
        row_count: usize,
    ) -> FfiResult {
        self.iter.mark_changed_range(col, start_row, row_count)
    }

    /// Marks single row `row` on writable column `col` as changed.
    #[inline]
    pub fn mark_changed(&mut self, col: usize, row: usize) -> FfiResult {
        self.iter.mark_changed(col, row)
    }

    /// Simultaneously borrows read-only dense column `read_col` (`&[R]`) and disjoint mutable dense
    /// column `write_col` (`&mut [W]`).
    #[inline]
    pub fn read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        self.iter
            .read_write_checked::<R, W>(read_col, None, write_col, None)
    }

    /// Simultaneously borrows read-only dense column `read_col` and disjoint mutable dense column
    /// `write_col`, verifying both `R`'s and `W`'s UUIDs.
    #[inline]
    pub fn component_read_write<R: GuestComponent, W: GuestComponent>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        let manifest = self.iter.host().manifest_name();
        let r_filter = guest_uuid_filter::<R>(manifest);
        let w_filter = guest_uuid_filter::<W>(manifest);
        self.iter
            .read_write_checked::<R, W>(read_col, Some(r_filter), write_col, Some(w_filter))
    }

    /// Simultaneously borrows two disjoint mutable dense columns (`col_a` and `col_b`).
    #[inline]
    pub fn write_2<W1: AbiPod, W2: AbiPod>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        self.iter
            .write_2_checked::<W1, W2>(col_a, None, col_b, None)
    }

    /// Simultaneously borrows two disjoint mutable dense columns (`col_a` and `col_b`), verifying
    /// `W1`'s and `W2`'s UUIDs.
    #[inline]
    pub fn component_write_2<W1: GuestComponent, W2: GuestComponent>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let manifest = self.iter.host().manifest_name();
        let filter_a = guest_uuid_filter::<W1>(manifest);
        let filter_b = guest_uuid_filter::<W2>(manifest);
        self.iter
            .write_2_checked::<W1, W2>(col_a, Some(filter_a), col_b, Some(filter_b))
    }

    /// Simultaneously borrows `[FfiEntity; len]` and mutable dense column `write_col`.
    #[inline]
    pub fn entities_and_column_mut<W: AbiPod>(
        &mut self,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &mut [W])> {
        self.iter
            .entities_and_column_mut_checked::<W>(write_col, None)
    }

    /// Simultaneously borrows `[FfiEntity; len]` and mutable dense column `write_col`, verifying
    /// `W`'s UUID.
    #[inline]
    pub fn entities_and_component_mut<W: GuestComponent>(
        &mut self,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &mut [W])> {
        let filter = guest_uuid_filter::<W>(self.iter.host().manifest_name());
        self.iter
            .entities_and_column_mut_checked::<W>(write_col, Some(filter))
    }

    /// Simultaneously borrows `[FfiEntity; len]`, read-only dense column `read_col`, and disjoint
    /// mutable dense column `write_col`.
    #[inline]
    pub fn entities_and_read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        self.iter
            .entities_and_read_write_checked::<R, W>(read_col, None, write_col, None)
    }

    /// Simultaneously borrows `[FfiEntity; len]`, read-only dense column `read_col`, and disjoint
    /// mutable dense column `write_col`, verifying `R`'s and `W`'s UUIDs.
    #[inline]
    pub fn entities_and_component_read_write<R: GuestComponent, W: GuestComponent>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        let manifest = self.iter.host().manifest_name();
        let r_filter = guest_uuid_filter::<R>(manifest);
        let w_filter = guest_uuid_filter::<W>(manifest);
        self.iter.entities_and_read_write_checked::<R, W>(
            read_col,
            Some(r_filter),
            write_col,
            Some(w_filter),
        )
    }

    /// Materializes indirect read-only sparse-set row pointers `[ *const u8; len ]` for column `col`
    ///.
    #[inline]
    pub fn rows<T: AbiPod>(&mut self, col: usize) -> Option<SparseRows<'_, T>> {
        self.iter.rows_checked::<T>(col, None)
    }

    /// Materializes indirect read-only sparse-set row pointers for `T: GuestComponent`, validating
    /// `T`'s UUID.
    #[inline]
    pub fn component_rows<T: GuestComponent>(&mut self, col: usize) -> Option<SparseRows<'_, T>> {
        let filter = guest_uuid_filter::<T>(self.iter.host().manifest_name());
        self.iter.rows_checked::<T>(col, Some(filter))
    }

    /// Materializes indirect mutable sparse-set row pointers `[ *mut u8; len ]` for column `col`
    ///, marking matched rows changed.
    #[inline]
    pub fn rows_mut<T: AbiPod>(&mut self, col: usize) -> Option<SparseRowsMut<'_, T>> {
        self.iter.rows_mut_checked::<T>(col, None)
    }

    /// Materializes indirect mutable sparse-set row pointers for `T: GuestComponent`, validating
    /// `T`'s UUID.
    #[inline]
    pub fn component_rows_mut<T: GuestComponent>(
        &mut self,
        col: usize,
    ) -> Option<SparseRowsMut<'_, T>> {
        let filter = guest_uuid_filter::<T>(self.iter.host().manifest_name());
        self.iter.rows_mut_checked::<T>(col, Some(filter))
    }

    /// Returns a low-level [`ChunkSlice`] view over dense read column `col` (useful with opaque
    /// VTable field accessors).
    #[inline]
    pub fn readonly_slice(&self, col: usize) -> ChunkSlice<'_> {
        ChunkSlice::from_validated(self.iter.readonly_slice(col))
    }

    /// Returns a low-level [`ChunkSliceMut`] view over dense write column `col` (marking ticks changed).
    #[inline]
    pub fn mutable_slice(&mut self, col: usize) -> ChunkSliceMut<'_> {
        ChunkSliceMut::from_validated(self.iter.mutable_slice(col))
    }

    /// Returns an untracked [`ChunkSliceMut`] view over dense write column `col`.
    #[inline]
    pub fn mutable_slice_untracked(&mut self, col: usize) -> ChunkSliceMut<'_> {
        ChunkSliceMut::from_validated(self.iter.mutable_slice_untracked(col))
    }

    /// Borrows singleton resource `R` (`R: AbiPod`) declared at `term_index` inside this query.
    #[inline]
    pub fn resource<R: AbiPod>(&self, term_index: usize) -> Option<&R> {
        self.iter.resource_checked::<R>(term_index, None)
    }

    /// Borrows singleton resource `R` (`R: GuestComponent`) declared at `term_index`, validating
    /// `R`'s UUID.
    #[inline]
    pub fn component_resource<R: GuestComponent>(&self, term_index: usize) -> Option<&R> {
        let filter = guest_uuid_filter::<R>(self.iter.host().manifest_name());
        self.iter.resource_checked::<R>(term_index, Some(filter))
    }

    /// Mutably borrows singleton resource `R` (`R: AbiPod`) declared with `RESOURCE_WRITE` at `term_index`.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, term_index: usize) -> Option<&mut R> {
        self.iter.resource_mut_checked::<R>(term_index, None)
    }

    /// Mutably borrows singleton resource `R` (`R: GuestComponent`) at `term_index`, validating
    /// `R`'s UUID.
    #[inline]
    pub fn component_resource_mut<R: GuestComponent>(
        &mut self,
        term_index: usize,
    ) -> Option<&mut R> {
        let filter = guest_uuid_filter::<R>(self.iter.host().manifest_name());
        self.iter
            .resource_mut_checked::<R>(term_index, Some(filter))
    }

    /// Dispenses all column slices declared by [`QueryTuple`] `Q` from this chunk without magic
    /// column index literals.
    #[inline]
    pub fn typed<Q: QueryTuple>(&mut self) -> Result<Q::Slices<'_>, FfiResult> {
        let mut dispenser = ChunkColumnDispenser::new(self.iter);
        Q::fetch_slices(&mut dispenser)
    }

    /// Drains unread POD messages (`M: AbiPod`) from `term_index`.
    #[inline]
    pub fn read_pod_messages<M: AbiPod>(&mut self, term_index: usize) -> Result<Vec<M>, FfiResult> {
        self.iter.drain_messages_checked::<M>(term_index, None)
    }

    /// Drains unread messages (`M: GuestComponent`) from `term_index`, validating `M`'s UUID.
    #[inline]
    pub fn read_messages<M: GuestComponent>(
        &mut self,
        term_index: usize,
    ) -> Result<Vec<M>, FfiResult> {
        let filter = guest_uuid_filter::<M>(self.iter.host().manifest_name());
        self.iter
            .drain_messages_checked::<M>(term_index, Some(filter))
    }

    /// Sends a POD message (`M: AbiPod`) on `term_index` declared with `MESSAGE_WRITE`.
    #[inline]
    pub fn send_pod_message<M: AbiPod>(&mut self, term_index: usize, msg: &M) -> FfiResult {
        self.iter.send_message_checked::<M>(term_index, None, msg)
    }

    /// Sends a message (`M: GuestComponent`) on `term_index`, validating `M`'s UUID.
    #[inline]
    pub fn send_message<M: GuestComponent>(&mut self, term_index: usize, msg: &M) -> FfiResult {
        let filter = guest_uuid_filter::<M>(self.iter.host().manifest_name());
        self.iter
            .send_message_checked::<M>(term_index, Some(filter), msg)
    }

    /// Drains unread VTable-backed non-POD messages (`M: ModComponent + Default`) from
    /// `term_index`.
    #[inline]
    pub fn read_non_pod_messages<M: ModComponent + Default>(
        &mut self,
        term_index: usize,
    ) -> Result<Vec<M>, FfiResult> {
        self.iter
            .drain_non_pod_messages_checked::<M>(term_index, Some(UuidFilter::exact(M::UUID)))
    }

    /// Reads up to `max_count` unread VTable-backed non-POD messages (`M: ModComponent + Default`)
    /// from `term_index`.
    #[inline]
    pub fn read_non_pod_messages_batch<M: ModComponent + Default>(
        &mut self,
        term_index: usize,
        max_count: usize,
    ) -> Result<Vec<M>, FfiResult> {
        self.iter.read_non_pod_messages_checked::<M>(
            term_index,
            Some(UuidFilter::exact(M::UUID)),
            max_count,
        )
    }

    /// Sends a VTable-backed non-POD message (`M: ModComponent`) on `term_index` declared with
    /// `MESSAGE_WRITE`.
    #[inline]
    pub fn send_non_pod_message<M: ModComponent>(
        &mut self,
        term_index: usize,
        msg: &M,
    ) -> FfiResult {
        self.iter.send_non_pod_message_checked::<M>(
            term_index,
            Some(UuidFilter::exact(M::UUID)),
            msg,
        )
    }
}
