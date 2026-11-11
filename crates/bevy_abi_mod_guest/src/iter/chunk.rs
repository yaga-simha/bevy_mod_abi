//! Scoped view over a single non-empty archetype chunk.

use core::ffi::CStr;

use bevy_abi_mod_sys::{AbiPod, FfiEntity, FfiResult};

use crate::iter::chunk_iter::GuestChunkIter;
use crate::iter::slice::{ChunkSlice, ChunkSliceMut};

/// Scoped handle to a single non-empty chunk yielded by [`GuestChunkIter::next_chunk_view`].
///
/// # Compile-Fail Safety Checks
///
/// `bool` is rejected as a column element type because byte values `2..=255` are undefined
/// behavior in Rust (`bool` does not implement [`AbiPod`]):
///
/// ```compile_fail
/// use bevy_abi_mod_guest::Chunk;
///
/// fn read_bool_column(chunk: &Chunk<'_, '_>) {
///     let _ = chunk.column::<bool>(0);
/// }
/// ```
///
/// `char` is likewise rejected because surrogate and out-of-range code points are invalid bit
/// patterns:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::Chunk;
///
/// fn write_char_column(chunk: &mut Chunk<'_, '_>) {
///     let _ = chunk.column_mut::<char>(0);
/// }
/// ```
///
/// Holding an immutable borrow from [`Chunk::column`] while taking a mutable borrow via
/// [`Chunk::column_mut`] is rejected by the borrow checker (use [`Chunk::read_write`] for
/// validated disjoint split borrows):
///
/// ```compile_fail
/// use bevy_abi_mod_guest::Chunk;
///
/// fn aliasing_columns(chunk: &mut Chunk<'_, '_>) {
///     let vel = chunk.column::<f32>(1).unwrap();
///     let pos = chunk.column_mut::<f32>(0).unwrap();
///     pos[0] += vel[0];
/// }
/// ```
pub struct Chunk<'iter, 'api> {
    iter: &'iter mut GuestChunkIter<'api>,
}

impl<'iter, 'api> Chunk<'iter, 'api> {
    #[inline]
    pub(crate) fn new(iter: &'iter mut GuestChunkIter<'api>) -> Self {
        Self { iter }
    }

    /// Number of entities in this chunk (guaranteed `>= 1` when yielded by `next_chunk_view`).
    #[inline]
    pub const fn len(&self) -> usize {
        self.iter.current_len()
    }

    /// Alias for [`Chunk::len`].
    #[inline]
    pub const fn count(&self) -> usize {
        self.iter.current_len()
    }

    /// Returns `true` if this chunk has zero entities.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.iter.current_len() == 0
    }

    /// Returns the validated slice of [`FfiEntity`] handles for all rows in this chunk.
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        self.iter.entities_slice()
    }

    /// Returns an untyped bounds-checked read-only [`ChunkSlice`] for column `index`.
    #[inline]
    pub fn readonly_slice(&self, index: usize) -> ChunkSlice<'_> {
        self.iter.readonly_slice(index)
    }

    /// Returns an untyped bounds-checked mutable [`ChunkSliceMut`] for column `index`.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize) -> ChunkSliceMut<'_> {
        self.iter.mutable_slice(index)
    }

    /// Borrows a typed read-only slice `&[T]` for column `index`.
    #[inline]
    pub fn column<T: AbiPod>(&self, index: usize) -> Option<&[T]> {
        self.iter.readonly_slice(index).as_typed_slice::<T>()
    }

    /// Borrows a typed mutable slice `&mut [T]` for column `index`.
    #[inline]
    pub fn column_mut<T: AbiPod>(&mut self, index: usize) -> Option<&mut [T]> {
        self.iter.mutable_slice(index).into_typed_slice::<T>()
    }

    /// Simultaneously borrows the chunk's [`FfiEntity`] slice and one typed mutable column `&mut [T]`.
    #[inline]
    pub fn entities_and_column_mut<T: AbiPod>(
        &mut self,
        index: usize,
    ) -> Option<(&[FfiEntity], &mut [T])> {
        let (entities, _, [rw]) = self.iter.split_slices([], [index])?;
        let typed = rw.into_typed_slice::<T>()?;
        Some((entities, typed))
    }

    /// Simultaneously borrows `entities`, `R` read-only [`ChunkSlice`]s, and `W` disjoint [`ChunkSliceMut`]s.
    #[allow(clippy::type_complexity)]
    #[inline]
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
    ) -> Option<(&[FfiEntity], [ChunkSlice<'_>; R], [ChunkSliceMut<'_>; W])> {
        self.iter.split_slices(read_cols, write_cols)
    }

    /// Simultaneously borrows `N` disjoint mutable [`ChunkSliceMut`] columns.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
    ) -> Option<[ChunkSliceMut<'_>; N]> {
        self.iter.columns_mut(indices)
    }

    /// Simultaneously borrows one typed read-only column `&[R]` and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        self.iter.read_write::<R, W>(read_col, write_col)
    }

    /// Simultaneously borrows `entities`, one typed read-only column `&[R]`, and one disjoint typed mutable column `&mut [W]`.
    #[inline]
    pub fn entities_and_read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        let (entities, [ro], [rw]) = self.iter.split_slices([read_col], [write_col])?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((entities, r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: AbiPod, W2: AbiPod>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        self.iter.write_2::<W1, W2>(col_a, col_b)
    }

    /// Explicitly marks `row` in column `index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        self.iter.mark_changed(index, row)
    }

    /// Returns `Some(ptr)` when an `OPTIONAL_READ` column is present on this chunk's archetype.
    #[inline]
    pub fn optional_column_ptr(&self, index: usize) -> Option<*const u8> {
        self.iter.optional_column_ptr(index)
    }

    /// Returns `Some(ptr)` and marks ticks when an `OPTIONAL_WRITE` column is present on this chunk.
    #[inline]
    pub fn optional_column_mut_ptr(&mut self, index: usize) -> Option<*mut u8> {
        self.iter.optional_column_mut_ptr(index)
    }

    /// Returns `Some(&[T])` when an `OPTIONAL_READ` [`AbiPod`] column is present on this chunk.
    #[inline]
    pub fn optional_column<T: AbiPod>(&self, index: usize) -> Option<&[T]> {
        self.iter
            .optional_readonly_slice(index)?
            .as_typed_slice::<T>()
    }

    /// Returns `Some(&mut [T])` and marks ticks when an `OPTIONAL_WRITE` [`AbiPod`] column is present on this chunk.
    #[inline]
    pub fn optional_column_mut<T: AbiPod>(&mut self, index: usize) -> Option<&mut [T]> {
        self.iter
            .optional_mutable_slice(index)?
            .into_typed_slice::<T>()
    }

    /// Simultaneously borrows an optional read-only column (`Option<&[OptR]>`) and a required
    /// disjoint mutable column (`&mut [W]`) for this chunk.
    #[inline]
    pub fn optional_read_write<OptR: AbiPod, W: AbiPod>(
        &mut self,
        opt_read_col: usize,
        write_col: usize,
    ) -> Option<(Option<&[OptR]>, &mut [W])> {
        self.iter
            .optional_read_write::<OptR, W>(opt_read_col, write_col)
    }

    /// Reads an exposed [`AbiPod`] Bevy `Resource` (`Res<R>`) while inspecting this chunk.
    #[inline]
    pub fn resource<R: AbiPod>(&self, resource_name: &CStr) -> Option<&R> {
        self.iter.resource::<R>(resource_name)
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` while inspecting this chunk.
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        self.iter.resource_ptr(resource_name)
    }
}
