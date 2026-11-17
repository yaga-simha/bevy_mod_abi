//! Safe dense column slice wrappers (`ChunkSlice` and `ChunkSliceMut`).

use bevy_abi_mod_guest_sys::{ValidatedSlice, ValidatedSliceMut};
use bevy_abi_mod_sys::{AbiPod, ComponentUuid};

use crate::component::{GuestComponent, guest_uuid_filter};

/// Safe read-only view over a dense archetype column for the current chunk.
///
/// Carries optional [`BamColumnInfo`](bevy_abi_mod_sys::BamColumnInfo) layout and UUID metadata
/// so [`Self::as_typed_slice`] and [`Self::as_component_slice`] reject mismatched element sizes,
/// alignments, or component UUIDs by returning `None`.
///
/// Constructing a `ChunkSlice` from raw pointers without `unsafe` is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkSlice;
///
/// let data = [1.0f32, 2.0f32];
/// let _s = ChunkSlice::from_raw_parts(data.as_ptr().cast::<u8>(), 2, 4);
/// ```
///
/// Casting to non-`AbiPod` types (`bool`, `String`, `usize`, `u128`) is rejected at compile time:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkSlice;
///
/// fn bad(slice: &ChunkSlice<'_>) {
///     let _ = slice.as_typed_slice::<bool>();
/// }
/// ```
pub struct ChunkSlice<'a> {
    inner: ValidatedSlice<'a>,
}

impl<'a> ChunkSlice<'a> {
    /// Wraps an already-validated [`ValidatedSlice`].
    #[inline]
    pub const fn from_validated(inner: ValidatedSlice<'a>) -> Self {
        Self { inner }
    }

    /// Returns the number of elements in this column view (`0` if absent or invalid).
    #[inline]
    pub const fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if `self.len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the known element byte stride (`0` if unspecified or zero-sized).
    #[inline]
    pub const fn stride(&self) -> usize {
        self.inner.stride()
    }

    /// Returns the column's [`ComponentUuid`] if constructed with [`BamColumnInfo`](bevy_abi_mod_sys::BamColumnInfo).
    #[inline]
    pub const fn uuid(&self) -> Option<ComponentUuid> {
        self.inner.uuid()
    }

    /// Returns the raw base byte pointer for FFI or VTable field-accessor calls.
    #[inline]
    pub const fn as_ptr(&self) -> *const u8 {
        self.inner.as_ptr()
    }

    /// Computes a bounds-checked element pointer at `index` using `elem_stride` bytes per element.
    #[inline]
    pub fn get_ptr(&self, index: usize, elem_stride: usize) -> Option<*const u8> {
        self.inner.get_ptr(index, elem_stride)
    }

    /// Casts this column view to `&[T]` (`T: AbiPod`), validating size and alignment.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&'a [T]> {
        self.inner.as_typed_slice::<T>()
    }

    /// Casts this column view to `&[T]` (`T: GuestComponent`), validating `T`'s UUID (including
    /// manifest-qualified names), size, and alignment.
    #[inline]
    pub fn as_component_slice<T: GuestComponent>(&self) -> Option<&'a [T]> {
        let filter = guest_uuid_filter::<T>(self.inner.manifest_name());
        self.inner.as_typed_slice_checked::<T>(Some(filter))
    }
}

impl<'a> From<ValidatedSlice<'a>> for ChunkSlice<'a> {
    #[inline]
    fn from(inner: ValidatedSlice<'a>) -> Self {
        Self::from_validated(inner)
    }
}

/// Safe mutable view over a dense archetype column for the current chunk.
///
/// Mutable slice borrows are tied to `&mut self` so safe Rust cannot create aliasing mutable
/// slices over the same column:
///
/// ```compile_fail
/// use bevy_abi_mod_guest::ChunkSliceMut;
///
/// fn alias_attempt(mut col: ChunkSliceMut<'_>) {
///     let a = col.as_typed_slice_mut::<f32>().unwrap();
///     let b = col.as_typed_slice_mut::<f32>().unwrap();
///     a[0] = b[0];
/// }
/// ```
pub struct ChunkSliceMut<'a> {
    inner: ValidatedSliceMut<'a>,
}

impl<'a> ChunkSliceMut<'a> {
    /// Wraps an already-validated [`ValidatedSliceMut`].
    #[inline]
    pub const fn from_validated(inner: ValidatedSliceMut<'a>) -> Self {
        Self { inner }
    }

    /// Returns the number of elements in this column view (`0` if absent or invalid).
    #[inline]
    pub const fn len(&self) -> usize {
        self.inner.len()
    }

    /// Returns `true` if `self.len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// Returns the known element byte stride (`0` if unspecified or zero-sized).
    #[inline]
    pub const fn stride(&self) -> usize {
        self.inner.stride()
    }

    /// Returns the column's [`ComponentUuid`] if constructed with [`BamColumnInfo`](bevy_abi_mod_sys::BamColumnInfo).
    #[inline]
    pub const fn uuid(&self) -> Option<ComponentUuid> {
        self.inner.uuid()
    }

    /// Returns the raw read-only base byte pointer.
    #[inline]
    pub const fn as_ptr(&self) -> *const u8 {
        self.inner.as_ptr()
    }

    /// Returns the raw mutable base byte pointer.
    #[inline]
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.inner.as_mut_ptr()
    }

    /// Computes a bounds-checked read-only element pointer at `index`.
    #[inline]
    pub fn get_ptr(&self, index: usize, elem_stride: usize) -> Option<*const u8> {
        self.inner.get_ptr(index, elem_stride)
    }

    /// Computes a bounds-checked mutable element pointer at `index`.
    #[inline]
    pub fn get_mut_ptr(&mut self, index: usize, elem_stride: usize) -> Option<*mut u8> {
        self.inner.get_mut_ptr(index, elem_stride)
    }

    /// Borrows this column view immutably as `&[T]` (`T: AbiPod`), tied to `&self`.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&[T]> {
        self.inner.as_typed_slice::<T>()
    }

    /// Borrows this column view immutably as `&[T]` (`T: GuestComponent`), validating `T`'s UUID,
    /// size, and alignment.
    #[inline]
    pub fn as_component_slice<T: GuestComponent>(&self) -> Option<&[T]> {
        let filter = guest_uuid_filter::<T>(self.inner.manifest_name());
        self.inner.as_typed_slice_checked::<T>(Some(filter))
    }

    /// Borrows this column view mutably as `&mut [T]` (`T: AbiPod`), tied to `&mut self`.
    #[inline]
    pub fn as_typed_slice_mut<T: AbiPod>(&mut self) -> Option<&mut [T]> {
        self.inner.as_typed_slice_mut::<T>()
    }

    /// Borrows this column view mutably as `&mut [T]` (`T: GuestComponent`), validating `T`'s UUID,
    /// size, and alignment.
    #[inline]
    pub fn as_component_slice_mut<T: GuestComponent>(&mut self) -> Option<&mut [T]> {
        let filter = guest_uuid_filter::<T>(self.inner.manifest_name());
        self.inner.as_typed_slice_mut_checked::<T>(Some(filter))
    }
}

impl<'a> From<ValidatedSliceMut<'a>> for ChunkSliceMut<'a> {
    #[inline]
    fn from(inner: ValidatedSliceMut<'a>) -> Self {
        Self::from_validated(inner)
    }
}
