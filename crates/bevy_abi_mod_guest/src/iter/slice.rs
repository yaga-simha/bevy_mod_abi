//! Bounds-checked read-only and mutable column slice views.

use bevy_abi_mod_guest_sys::{
    ValidatedSlice, ValidatedSliceAdapter, ValidatedSliceMut, ValidatedSliceMutAdapter,
};
use bevy_abi_mod_sys::AbiPod;

/// Read-only chunk slice view for iterating components in an archetype table chunk.
///
/// Obtain a `ChunkSlice` safely from [`Chunk::readonly_slice`](crate::Chunk::readonly_slice) or
/// [`GuestChunkIter::readonly_slice`](crate::GuestChunkIter::readonly_slice).
///
/// # Compile-Fail Safety Checks
///
/// Constructing `ChunkSlice` directly from a raw pointer without `unsafe` is rejected:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// let data = [10u32, 20, 30];
/// let _slice = ChunkSlice::new(data.as_ptr().cast::<u8>(), 4, 3);
/// ```
///
/// Reinterpreting column bytes as a type with invalid bit-pattern niches such as
/// [`core::num::NonZeroU32`] is rejected because `NonZeroU32` does not implement [`AbiPod`]:
///
/// ```compile_fail
/// use core::num::NonZeroU32;
/// use bevy_abi_mod_guest::ChunkSlice;
///
/// fn bad_view(slice: ChunkSlice<'_>) {
///     let _ = slice.as_typed_slice::<NonZeroU32>();
/// }
/// ```
#[derive(Clone, Copy, Debug)]
pub struct ChunkSlice<'a> {
    inner: ValidatedSlice<'a>,
}

impl<'a> ChunkSlice<'a> {
    #[inline]
    pub(crate) fn from_validated(inner: ValidatedSlice<'a>) -> Self {
        Self { inner }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub const fn count(&self) -> usize {
        self.inner.count()
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub const fn stride(&self) -> usize {
        self.inner.stride()
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns a pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr(&self, index: usize) -> Option<*const u8> {
        self.inner.get_ptr(index)
    }

    /// Converts this chunk slice into a typed `&'a [T]` slice of [`AbiPod`] elements.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&'a [T]> {
        self.inner.as_typed_slice::<T>()
    }
}

impl<'a> ValidatedSliceAdapter<'a> for ChunkSlice<'a> {
    #[inline]
    fn from_validated_slice(inner: ValidatedSlice<'a>) -> Self {
        Self { inner }
    }
}

/// Mutable chunk slice view for iterating components in an archetype table chunk.
///
/// Obtain a `ChunkSliceMut` safely from [`Chunk::mutable_slice`](crate::Chunk::mutable_slice) or
/// [`GuestChunkIter::mutable_slice`](crate::GuestChunkIter::mutable_slice).
///
/// # Compile-Fail Safety Checks
///
/// Constructing `ChunkSliceMut` directly from a raw pointer without `unsafe` is rejected:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::prelude::*;
///
/// let mut data = [10u32, 20, 30];
/// let _slice = ChunkSliceMut::new(data.as_mut_ptr().cast::<u8>(), 4, 3);
/// ```
///
/// Reinterpreting column bytes as a fieldless `enum` is rejected because Rust `enum`s do not
/// implement [`AbiPod`]:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkSliceMut;
///
/// #[repr(C)]
/// #[derive(Clone, Copy)]
/// enum Team {
///     Red = 0,
///     Blue = 1,
/// }
///
/// fn bad_enum_view(slice: &mut ChunkSliceMut<'_>) {
///     let _ = slice.as_typed_slice::<Team>();
/// }
/// ```
#[derive(Debug)]
pub struct ChunkSliceMut<'a> {
    inner: ValidatedSliceMut<'a>,
}

impl<'a> ChunkSliceMut<'a> {
    #[inline]
    pub(crate) fn from_validated(inner: ValidatedSliceMut<'a>) -> Self {
        Self { inner }
    }

    /// Returns the number of entity elements in the current chunk.
    #[inline]
    pub const fn count(&self) -> usize {
        self.inner.count()
    }

    /// Returns the byte stride between consecutive elements in the column.
    #[inline]
    pub const fn stride(&self) -> usize {
        self.inner.stride()
    }

    /// Returns `true` if the chunk slice contains zero elements or points to null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns a mutable pointer to the element at the specified index within the chunk.
    #[inline]
    pub fn get_ptr_mut(&mut self, index: usize) -> Option<*mut u8> {
        self.inner.get_ptr_mut(index)
    }

    /// Reborrows this mutable chunk slice as a typed `&mut [T]` slice of [`AbiPod`] elements.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&mut self) -> Option<&mut [T]> {
        self.inner.as_typed_slice::<T>()
    }

    /// Consumes this `ChunkSliceMut<'a>` and converts it into `&'a mut [T]` for `T: AbiPod`.
    #[inline]
    pub fn into_typed_slice<T: AbiPod>(self) -> Option<&'a mut [T]> {
        self.inner.into_typed_slice::<T>()
    }
}

impl<'a> ValidatedSliceMutAdapter<'a> for ChunkSliceMut<'a> {
    #[inline]
    fn from_validated_slice_mut(inner: ValidatedSliceMut<'a>) -> Self {
        Self { inner }
    }
}
