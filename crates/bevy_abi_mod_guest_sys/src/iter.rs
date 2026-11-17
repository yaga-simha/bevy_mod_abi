//! Validated chunk iteration, `BamColumnInfo` layout/UUID verification, dense slices, indirect
//! sparse-set row views, untracked mutation + `mark_changed_range`, and singleton resource access.

use core::{ffi::c_void, marker::PhantomData, mem::MaybeUninit, ops::Index};

use bevy_abi_mod_sys::{
    AbiPod, AccessMode, BamColumnInfo, BamComponent, BamHostApi, BamIter, BamMsgReadBatchFn,
    BamMsgSendFn, BamParForEachChunkFn, ComponentUuid, FfiEntity, FfiResult, MAX_TERMS_PER_QUERY,
};

use crate::{host::Host, panic::catch_guest_result};

/// Expected [`ComponentUuid`] predicate for `C-13` typed column and resource introspection.
///
/// Supports either a single exact UUID or a primary manifest-qualified UUID with an unqualified
/// fallback (matching `C-10` auto-namespaced `GuestComponent` resolution).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UuidFilter {
    primary: ComponentUuid,
    fallback: Option<ComponentUuid>,
}

impl UuidFilter {
    /// Matches only `uuid`.
    #[inline]
    pub const fn exact(uuid: ComponentUuid) -> Self {
        Self {
            primary: uuid,
            fallback: None,
        }
    }

    /// Matches either `primary` (e.g. `"<manifest>::<Name>"`) or `fallback` (e.g. `"<Name>"`).
    #[inline]
    pub const fn with_fallback(primary: ComponentUuid, fallback: ComponentUuid) -> Self {
        Self {
            primary,
            fallback: Some(fallback),
        }
    }

    /// Returns `true` if `actual` matches `primary` or `fallback`.
    #[inline]
    pub fn matches(self, actual: ComponentUuid) -> bool {
        actual == self.primary || self.fallback.is_some_and(|fb| actual == fb)
    }
}

impl From<ComponentUuid> for UuidFilter {
    #[inline]
    fn from(uuid: ComponentUuid) -> Self {
        Self::exact(uuid)
    }
}

#[inline]
fn validate_dense_read_info<T: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !info.mode.has_entity_column_data() || info.is_resource() || info.is_sparse_set() {
        return false;
    }
    if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_dense_write_info<T: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !info.mode.is_mutable()
        || !info.mode.has_entity_column_data()
        || info.is_immutable()
        || info.is_resource()
        || info.is_sparse_set()
    {
        return false;
    }
    if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_rows_read_info<T: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !info.mode.has_entity_column_data() || info.is_resource() {
        return false;
    }
    if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_rows_write_info<T: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !info.mode.is_mutable()
        || !info.mode.has_entity_column_data()
        || info.is_immutable()
        || info.is_resource()
    {
        return false;
    }
    if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_resource_read_info<R: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !info.mode.is_resource() {
        return false;
    }
    if info.size != core::mem::size_of::<R>() || info.align != core::mem::align_of::<R>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_resource_write_info<R: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if info.mode != AccessMode::RESOURCE_WRITE || info.is_immutable() {
        return false;
    }
    if info.size != core::mem::size_of::<R>() || info.align != core::mem::align_of::<R>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
unsafe fn raw_dense_slice<'a, T: AbiPod>(ptr: *const u8, len: usize) -> Option<&'a [T]> {
    let elem_size = core::mem::size_of::<T>();
    if elem_size == 0 {
        if ptr.is_null() && len > 0 {
            return None;
        }
        // SAFETY: `T: AbiPod` is a zero-sized type; `NonNull::dangling()` is aligned and valid for
        // any `len` without dereferencing memory.
        return Some(unsafe {
            core::slice::from_raw_parts(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
        });
    }
    if ptr.is_null() {
        return None;
    }
    if len == 0 {
        return Some(&[]);
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    let byte_len = len.checked_mul(elem_size)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    // SAFETY: Verified non-null, aligned for `T`, `byte_len <= isize::MAX`, and caller guarantees
    // `ptr` points to `len` contiguous initialized `T` elements valid for `'a`.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<T>(), len) })
}

#[inline]
unsafe fn raw_dense_slice_mut<'a, T: AbiPod>(ptr: *mut u8, len: usize) -> Option<&'a mut [T]> {
    let elem_size = core::mem::size_of::<T>();
    if elem_size == 0 {
        if ptr.is_null() && len > 0 {
            return None;
        }
        // SAFETY: `T: AbiPod` is zero-sized; `NonNull::dangling()` forms a sound mutable ZST slice.
        return Some(unsafe {
            core::slice::from_raw_parts_mut(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
        });
    }
    if ptr.is_null() {
        return None;
    }
    if len == 0 {
        return Some(&mut []);
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    let byte_len = len.checked_mul(elem_size)?;
    if byte_len > isize::MAX as usize {
        return None;
    }
    // SAFETY: Verified non-null, aligned for `T`, `byte_len <= isize::MAX`, and caller guarantees
    // exclusive mutable access to `len` contiguous `T` elements valid for `'a`.
    Some(unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), len) })
}

/// Validated read-only dense column view carrying optional [`BamColumnInfo`] layout/UUID metadata.
pub struct ValidatedSlice<'a> {
    ptr: *const u8,
    len: usize,
    expected_size: Option<usize>,
    expected_align: Option<usize>,
    uuid: Option<ComponentUuid>,
    manifest_name: &'a str,
    valid: bool,
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> ValidatedSlice<'a> {
    /// Constructs an empty/invalid slice whose typed conversions return `None` (or `&[]` if valid
    /// zero-length with matching layout).
    #[inline]
    pub const fn invalid() -> Self {
        Self {
            ptr: core::ptr::null(),
            len: 0,
            expected_size: None,
            expected_align: None,
            uuid: None,
            manifest_name: "",
            valid: false,
            _marker: PhantomData,
        }
    }

    /// Constructs a [`ValidatedSlice`] from raw parts with an optional byte `stride` (`0` = unspecified).
    ///
    /// # Safety
    /// If `ptr` is non-null and `len > 0`, `ptr` must remain valid for shared reads of `len`
    /// contiguous elements of `stride` bytes for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: *const u8, len: usize, stride: usize) -> Self {
        Self {
            ptr,
            len,
            expected_size: if stride > 0 { Some(stride) } else { None },
            expected_align: None,
            uuid: None,
            manifest_name: "",
            valid: !ptr.is_null() || len == 0,
            _marker: PhantomData,
        }
    }

    /// Constructs a [`ValidatedSlice`] stamped with host [`BamColumnInfo`] layout and UUID metadata.
    ///
    /// # Safety
    /// If `ptr` is non-null and `len > 0`, `ptr` must remain valid for shared reads of `len`
    /// contiguous elements described by `info` for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts_with_info(
        ptr: *const u8,
        len: usize,
        info: BamColumnInfo,
    ) -> Self {
        let ok_mode =
            info.mode.has_entity_column_data() && !info.is_resource() && !info.is_sparse_set();
        Self {
            ptr,
            len,
            expected_size: Some(info.size),
            expected_align: Some(info.align),
            uuid: Some(info.uuid),
            manifest_name: "",
            valid: ok_mode && (!ptr.is_null() || (len == 0 && !info.mode.is_optional())),
            _marker: PhantomData,
        }
    }

    /// Stamps this slice with the owning mod instance's `manifest_name` for `C-10` auto-namespaced
    /// `GuestComponent` UUID matching.
    #[inline]
    pub const fn with_manifest_name(mut self, manifest_name: &'a str) -> Self {
        self.manifest_name = manifest_name;
        self
    }

    /// Returns the owning mod instance's manifest name (`""` if unspecified).
    #[inline]
    pub const fn manifest_name(&self) -> &'a str {
        self.manifest_name
    }

    /// Returns the number of elements in this chunk view (`0` if invalid/absent).
    #[inline]
    pub const fn len(&self) -> usize {
        if self.valid { self.len } else { 0 }
    }

    /// Returns `true` if `len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the raw base byte pointer.
    #[inline]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    /// Returns the known element byte stride (`0` if unknown or ZST).
    #[inline]
    pub const fn stride(&self) -> usize {
        match self.expected_size {
            Some(s) => s,
            None => 0,
        }
    }

    /// Returns the column's [`ComponentUuid`] if constructed with [`BamColumnInfo`].
    #[inline]
    pub const fn uuid(&self) -> Option<ComponentUuid> {
        self.uuid
    }

    /// Computes a bounds-checked element pointer at `index` using `elem_stride` bytes per element.
    #[inline]
    pub fn get_ptr(&self, index: usize, elem_stride: usize) -> Option<*const u8> {
        if !self.valid || self.ptr.is_null() || index >= self.len {
            return None;
        }
        if let Some(expected) = self.expected_size
            && expected != elem_stride
        {
            return None;
        }
        let byte_offset = index.checked_mul(elem_stride)?;
        let total = self.len.checked_mul(elem_stride)?;
        if total > isize::MAX as usize {
            return None;
        }
        // SAFETY: `index < self.len`, `total <= isize::MAX`, and `self.ptr` covers `total` bytes.
        Some(unsafe { self.ptr.add(byte_offset) })
    }

    /// Casts this column view to `&[T]` after verifying size, alignment, and optional `expected_uuid`.
    #[inline]
    pub fn as_typed_slice_checked<T: AbiPod>(
        &self,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&'a [T]> {
        if !self.valid {
            return None;
        }
        if let Some(filter) = expected_uuid {
            let actual_uuid = self.uuid?;
            if !filter.matches(actual_uuid) {
                return None;
            }
        }
        if let Some(sz) = self.expected_size
            && sz != core::mem::size_of::<T>()
        {
            return None;
        }
        if let Some(al) = self.expected_align
            && al != core::mem::align_of::<T>()
        {
            return None;
        }
        if self.len == 0 && !self.ptr.is_null() {
            return Some(&[]);
        }
        if self.len == 0 && core::mem::size_of::<T>() == 0 {
            return Some(&[]);
        }
        if self.len == 0 && self.expected_size.is_none() {
            return Some(&[]);
        }
        // SAFETY: Layout/UUID checks succeeded and `self.ptr` covers `self.len` elements for `'a`.
        unsafe { raw_dense_slice::<T>(self.ptr, self.len) }
    }

    /// Casts this column view to `&[T]` (`T: AbiPod`), validating size and alignment.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&'a [T]> {
        self.as_typed_slice_checked::<T>(None)
    }
}

/// Validated mutable dense column view carrying optional [`BamColumnInfo`] layout/UUID metadata.
pub struct ValidatedSliceMut<'a> {
    ptr: *mut u8,
    len: usize,
    expected_size: Option<usize>,
    expected_align: Option<usize>,
    uuid: Option<ComponentUuid>,
    manifest_name: &'a str,
    valid: bool,
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> ValidatedSliceMut<'a> {
    /// Constructs an invalid mutable slice whose typed conversions return `None`.
    #[inline]
    pub const fn invalid() -> Self {
        Self {
            ptr: core::ptr::null_mut(),
            len: 0,
            expected_size: None,
            expected_align: None,
            uuid: None,
            manifest_name: "",
            valid: false,
            _marker: PhantomData,
        }
    }

    /// Constructs a [`ValidatedSliceMut`] from raw parts with an optional byte `stride` (`0` = unspecified).
    ///
    /// # Safety
    /// If `ptr` is non-null and `len > 0`, `ptr` must remain valid for exclusive mutable reads and
    /// writes of `len` contiguous elements of `stride` bytes for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts(ptr: *mut u8, len: usize, stride: usize) -> Self {
        Self {
            ptr,
            len,
            expected_size: if stride > 0 { Some(stride) } else { None },
            expected_align: None,
            uuid: None,
            manifest_name: "",
            valid: !ptr.is_null() || len == 0,
            _marker: PhantomData,
        }
    }

    /// Constructs a [`ValidatedSliceMut`] stamped with host [`BamColumnInfo`] layout and UUID metadata.
    ///
    /// # Safety
    /// If `ptr` is non-null and `len > 0`, `ptr` must remain valid for exclusive mutable access to
    /// `len` contiguous elements described by `info` for lifetime `'a`.
    #[inline]
    pub const unsafe fn from_raw_parts_with_info(
        ptr: *mut u8,
        len: usize,
        info: BamColumnInfo,
    ) -> Self {
        let ok_mode = info.mode.is_mutable()
            && info.mode.has_entity_column_data()
            && !info.is_immutable()
            && !info.is_resource()
            && !info.is_sparse_set();
        Self {
            ptr,
            len,
            expected_size: Some(info.size),
            expected_align: Some(info.align),
            uuid: Some(info.uuid),
            manifest_name: "",
            valid: ok_mode && (!ptr.is_null() || (len == 0 && !info.mode.is_optional())),
            _marker: PhantomData,
        }
    }

    /// Stamps this slice with the owning mod instance's `manifest_name` for `C-10` auto-namespaced
    /// `GuestComponent` UUID matching.
    #[inline]
    pub const fn with_manifest_name(mut self, manifest_name: &'a str) -> Self {
        self.manifest_name = manifest_name;
        self
    }

    /// Returns the owning mod instance's manifest name (`""` if unspecified).
    #[inline]
    pub const fn manifest_name(&self) -> &'a str {
        self.manifest_name
    }

    /// Returns the number of elements in this chunk view (`0` if invalid/absent).
    #[inline]
    pub const fn len(&self) -> usize {
        if self.valid { self.len } else { 0 }
    }

    /// Returns `true` if `len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the raw base byte pointer.
    #[inline]
    pub const fn as_ptr(&self) -> *const u8 {
        self.ptr.cast_const()
    }

    /// Returns the raw mutable base byte pointer.
    #[inline]
    pub const fn as_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    /// Returns the known element byte stride (`0` if unknown or ZST).
    #[inline]
    pub const fn stride(&self) -> usize {
        match self.expected_size {
            Some(s) => s,
            None => 0,
        }
    }

    /// Returns the column's [`ComponentUuid`] if constructed with [`BamColumnInfo`].
    #[inline]
    pub const fn uuid(&self) -> Option<ComponentUuid> {
        self.uuid
    }

    /// Computes a bounds-checked read-only element pointer at `index`.
    #[inline]
    pub fn get_ptr(&self, index: usize, elem_stride: usize) -> Option<*const u8> {
        if !self.valid || self.ptr.is_null() || index >= self.len {
            return None;
        }
        if let Some(expected) = self.expected_size
            && expected != elem_stride
        {
            return None;
        }
        let byte_offset = index.checked_mul(elem_stride)?;
        let total = self.len.checked_mul(elem_stride)?;
        if total > isize::MAX as usize {
            return None;
        }
        // SAFETY: `index < self.len`, `total <= isize::MAX`, and `self.ptr` covers `total` bytes.
        Some(unsafe { self.ptr.add(byte_offset).cast_const() })
    }

    /// Computes a bounds-checked mutable element pointer at `index`.
    #[inline]
    pub fn get_mut_ptr(&mut self, index: usize, elem_stride: usize) -> Option<*mut u8> {
        if !self.valid || self.ptr.is_null() || index >= self.len {
            return None;
        }
        if let Some(expected) = self.expected_size
            && expected != elem_stride
        {
            return None;
        }
        let byte_offset = index.checked_mul(elem_stride)?;
        let total = self.len.checked_mul(elem_stride)?;
        if total > isize::MAX as usize {
            return None;
        }
        // SAFETY: `index < self.len`, `total <= isize::MAX`, and `self.ptr` covers `total` bytes.
        Some(unsafe { self.ptr.add(byte_offset) })
    }

    /// Borrows this column view as `&[T]` (tied to `&self`) after verifying size, alignment, and
    /// optional `expected_uuid`.
    #[inline]
    pub fn as_typed_slice_checked<T: AbiPod>(
        &self,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&[T]> {
        if !self.valid {
            return None;
        }
        if let Some(filter) = expected_uuid {
            let actual_uuid = self.uuid?;
            if !filter.matches(actual_uuid) {
                return None;
            }
        }
        if let Some(sz) = self.expected_size
            && sz != core::mem::size_of::<T>()
        {
            return None;
        }
        if let Some(al) = self.expected_align
            && al != core::mem::align_of::<T>()
        {
            return None;
        }
        if self.len == 0
            && (!self.ptr.is_null()
                || core::mem::size_of::<T>() == 0
                || self.expected_size.is_none())
        {
            return Some(&[]);
        }
        // SAFETY: Layout/UUID checks succeeded and `&self` borrows `self.ptr` immutably.
        unsafe { raw_dense_slice::<T>(self.ptr.cast_const(), self.len) }
    }

    /// Borrows this column view as `&[T]` (`T: AbiPod`), tied to `&self`.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&[T]> {
        self.as_typed_slice_checked::<T>(None)
    }

    /// Borrows this column view as `&mut [T]` (tied to `&mut self`) after verifying size,
    /// alignment, and optional `expected_uuid`.
    #[inline]
    pub fn as_typed_slice_mut_checked<T: AbiPod>(
        &mut self,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&mut [T]> {
        if !self.valid {
            return None;
        }
        if let Some(filter) = expected_uuid {
            let actual_uuid = self.uuid?;
            if !filter.matches(actual_uuid) {
                return None;
            }
        }
        if let Some(sz) = self.expected_size
            && sz != core::mem::size_of::<T>()
        {
            return None;
        }
        if let Some(al) = self.expected_align
            && al != core::mem::align_of::<T>()
        {
            return None;
        }
        if self.len == 0
            && (!self.ptr.is_null()
                || core::mem::size_of::<T>() == 0
                || self.expected_size.is_none())
        {
            return Some(&mut []);
        }
        // SAFETY: Layout/UUID checks succeeded and `&mut self` guarantees exclusive access.
        unsafe { raw_dense_slice_mut::<T>(self.ptr, self.len) }
    }

    /// Borrows this column view as `&mut [T]` (`T: AbiPod`), tied to `&mut self`.
    #[inline]
    pub fn as_typed_slice_mut<T: AbiPod>(&mut self) -> Option<&mut [T]> {
        self.as_typed_slice_mut_checked::<T>(None)
    }

    /// Consumes this wrapper and returns `&'a mut [T]` with the full chunk lifetime `'a`.
    #[inline]
    pub fn into_typed_slice_mut_checked<T: AbiPod>(
        self,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&'a mut [T]> {
        if !self.valid {
            return None;
        }
        if let Some(filter) = expected_uuid {
            let actual_uuid = self.uuid?;
            if !filter.matches(actual_uuid) {
                return None;
            }
        }
        if let Some(sz) = self.expected_size
            && sz != core::mem::size_of::<T>()
        {
            return None;
        }
        if let Some(al) = self.expected_align
            && al != core::mem::align_of::<T>()
        {
            return None;
        }
        if self.len == 0
            && (!self.ptr.is_null()
                || core::mem::size_of::<T>() == 0
                || self.expected_size.is_none())
        {
            return Some(&mut []);
        }
        // SAFETY: Consumes `self`, transferring exclusive `'a` access to the returned slice.
        unsafe { raw_dense_slice_mut::<T>(self.ptr, self.len) }
    }
}

/// Safe read-only view over an indirect `[ *const u8; chunk_len ]` sparse-set row pointer array.
pub struct SparseRows<'a, T: AbiPod> {
    ptrs: *const *const u8,
    len: usize,
    _marker: PhantomData<&'a [T]>,
}

// SAFETY: `T: AbiPod` is `Sync`, and `ptrs` points to host-owned read-only row pointers valid for `'a`.
unsafe impl<T: AbiPod> Send for SparseRows<'_, T> {}
// SAFETY: Shared access to `SparseRows` only yields `&T` references where `T: Sync`.
unsafe impl<T: AbiPod> Sync for SparseRows<'_, T> {}

impl<'a, T: AbiPod> SparseRows<'a, T> {
    /// Validates `ptrs[0..len]` and constructs a [`SparseRows`] view.
    ///
    /// When `allow_null_rows` is `false`, returns `None` if any row pointer is null or unaligned
    /// for `T`. When `allow_null_rows` is `true` (`OPTIONAL_READ`), null row pointers are accepted
    /// and yielded as `None` by [`Self::get`], while non-null unaligned pointers still return `None`.
    ///
    /// # Safety
    /// If `len > 0`, `ptrs` must be non-null, aligned to `*const u8`, and readable for `len`
    /// pointers for lifetime `'a`, where every non-null `ptrs[i]` points to an initialized `T`
    /// valid for shared reads during `'a`.
    pub unsafe fn from_raw_parts(
        ptrs: *const *const u8,
        len: usize,
        allow_null_rows: bool,
    ) -> Option<Self> {
        if len == 0 {
            return Some(Self {
                ptrs: core::ptr::NonNull::<*const u8>::dangling().as_ptr(),
                len: 0,
                _marker: PhantomData,
            });
        }
        if ptrs.is_null() || !(ptrs as usize).is_multiple_of(core::mem::align_of::<*const u8>()) {
            return None;
        }
        let align = core::mem::align_of::<T>();
        let elem_size = core::mem::size_of::<T>();
        for i in 0..len {
            // SAFETY: `ptrs` is non-null, aligned, and readable for `len` pointers per caller contract.
            let row_ptr = unsafe { *ptrs.add(i) };
            if row_ptr.is_null() {
                if !allow_null_rows {
                    return None;
                }
            } else if elem_size > 0 && !(row_ptr as usize).is_multiple_of(align) {
                return None;
            }
        }
        Some(Self {
            ptrs,
            len,
            _marker: PhantomData,
        })
    }

    /// Number of entity rows in the current chunk.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if `self.len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns `&T` for `row`, or `None` if `row >= self.len()` or the optional component is absent
    /// on that entity row.
    #[inline]
    pub fn get(&self, row: usize) -> Option<&T> {
        if row >= self.len {
            return None;
        }
        // SAFETY: `row < self.len`, and `from_raw_parts` verified `self.ptrs` is readable for `self.len`.
        let row_ptr = unsafe { *self.ptrs.add(row) };
        if row_ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: AbiPod` may be referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_ref() });
        }
        // SAFETY: `from_raw_parts` verified every non-null `row_ptr` is aligned to `T` and points
        // to an initialized `T` valid for `'a`.
        Some(unsafe { &*row_ptr.cast::<T>() })
    }

    /// Iterates over all present `&T` row values in entity order.
    #[inline]
    pub fn iter(&self) -> SparseRowsIter<'_, 'a, T> {
        SparseRowsIter {
            rows: self,
            index: 0,
        }
    }
}

impl<T: AbiPod> Index<usize> for SparseRows<'_, T> {
    type Output = T;

    #[inline]
    fn index(&self, index: usize) -> &Self::Output {
        self.get(index)
            .expect("sparse row index out of bounds or absent optional component")
    }
}

/// Iterator over present `&T` items in [`SparseRows`].
pub struct SparseRowsIter<'b, 'a, T: AbiPod> {
    rows: &'b SparseRows<'a, T>,
    index: usize,
}

impl<'b, T: AbiPod> Iterator for SparseRowsIter<'b, '_, T> {
    type Item = &'b T;

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        while self.index < self.rows.len {
            let idx = self.index;
            self.index += 1;
            if let Some(val) = self.rows.get(idx) {
                return Some(val);
            }
        }
        None
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, Some(self.rows.len.saturating_sub(self.index)))
    }
}

impl<'b, 'a, T: AbiPod> IntoIterator for &'b SparseRows<'a, T> {
    type Item = &'b T;
    type IntoIter = SparseRowsIter<'b, 'a, T>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

/// Safe mutable view over an indirect `[ *mut u8; chunk_len ]` sparse-set row pointer array.
///
/// Mutable row access (`get_mut` and `for_each_mut`) lends at most one `&mut T` at a time tied to
/// `&mut self`, preventing aliasing mutable borrows even if a faulty host returned duplicate row
/// pointers.
pub struct SparseRowsMut<'a, T: AbiPod> {
    ptrs: *const *mut u8,
    len: usize,
    _marker: PhantomData<&'a mut [T]>,
}

// SAFETY: `T: AbiPod` is `Send + Sync`, and `SparseRowsMut` holds exclusive access to the column's
// row pointers for `'a`.
unsafe impl<T: AbiPod> Send for SparseRowsMut<'_, T> {}
// SAFETY: Shared references (`&SparseRowsMut`) only yield `&T` (`T: Sync`).
unsafe impl<T: AbiPod> Sync for SparseRowsMut<'_, T> {}

impl<'a, T: AbiPod> SparseRowsMut<'a, T> {
    /// Validates `ptrs[0..len]` and constructs a [`SparseRowsMut`] view.
    ///
    /// # Safety
    /// If `len > 0`, `ptrs` must be non-null, aligned to `*mut u8`, and readable for `len` pointers
    /// for lifetime `'a`, where every non-null `ptrs[i]` points to an initialized `T` valid for
    /// exclusive mutable access during `'a`.
    pub unsafe fn from_raw_parts(
        ptrs: *const *mut u8,
        len: usize,
        allow_null_rows: bool,
    ) -> Option<Self> {
        if len == 0 {
            return Some(Self {
                ptrs: core::ptr::NonNull::<*mut u8>::dangling().as_ptr(),
                len: 0,
                _marker: PhantomData,
            });
        }
        if ptrs.is_null() || !(ptrs as usize).is_multiple_of(core::mem::align_of::<*mut u8>()) {
            return None;
        }
        let align = core::mem::align_of::<T>();
        let elem_size = core::mem::size_of::<T>();
        for i in 0..len {
            // SAFETY: `ptrs` is non-null, aligned, and readable for `len` pointers per caller contract.
            let row_ptr = unsafe { *ptrs.add(i) };
            if row_ptr.is_null() {
                if !allow_null_rows {
                    return None;
                }
            } else if elem_size > 0 && !(row_ptr as usize).is_multiple_of(align) {
                return None;
            }
        }
        Some(Self {
            ptrs,
            len,
            _marker: PhantomData,
        })
    }

    /// Number of entity rows in the current chunk.
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` if `self.len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Borrows row `row` immutably (`None` if out of bounds or absent on an optional column).
    #[inline]
    pub fn get(&self, row: usize) -> Option<&T> {
        if row >= self.len {
            return None;
        }
        // SAFETY: `row < self.len`, and `from_raw_parts` verified `self.ptrs` is readable.
        let row_ptr = unsafe { *self.ptrs.add(row) };
        if row_ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: AbiPod` may be referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_ref() });
        }
        // SAFETY: `from_raw_parts` verified alignment and initialization; `&self` prevents concurrent mutation.
        Some(unsafe { &*row_ptr.cast_const().cast::<T>() })
    }

    /// Borrows row `row` mutably (`None` if out of bounds or absent on an optional column).
    #[inline]
    pub fn get_mut(&mut self, row: usize) -> Option<&mut T> {
        if row >= self.len {
            return None;
        }
        // SAFETY: `row < self.len`, and `from_raw_parts` verified `self.ptrs` is readable.
        let row_ptr = unsafe { *self.ptrs.add(row) };
        if row_ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<T>() == 0 {
            // SAFETY: Zero-sized `T: AbiPod` may be mutably referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<T>::dangling().as_mut() });
        }
        // SAFETY: `from_raw_parts` verified alignment and initialization; tying the borrow to
        // `&mut self` guarantees at most one `&mut T` exists at any instant.
        Some(unsafe { &mut *row_ptr.cast::<T>() })
    }

    /// Invokes `f(row_index, &mut T)` sequentially for every present row in `0..self.len()`.
    #[inline]
    pub fn for_each_mut(&mut self, mut f: impl FnMut(usize, &mut T)) {
        let len = self.len;
        for row in 0..len {
            if let Some(item) = self.get_mut(row) {
                f(row, item);
            }
        }
    }
}

/// Validated query chunk and singleton resource accessor wrapping `*mut BamIter`.
pub struct ValidatedIter<'a> {
    raw: *mut BamIter,
    host: &'a Host,
    current_len: usize,
}

impl<'a> ValidatedIter<'a> {
    /// Wraps a raw [`BamIter`] pointer obtained from `BamHostApi::sys_query`.
    ///
    /// A null `raw` pointer constructs an exhausted iterator yielding zero chunks.
    ///
    /// # Safety
    /// If `raw` is non-null, it must point to an active [`BamIter`] belonging to the current
    /// system invocation on `host` and must not outlive `'a`.
    #[inline]
    pub const unsafe fn from_raw(raw: *mut BamIter, host: &'a Host) -> Self {
        Self {
            raw,
            host,
            current_len: 0,
        }
    }

    /// Constructs a disabled/exhausted iterator (`raw = null`) yielding zero chunks.
    #[inline]
    pub const fn empty(host: &'a Host) -> Self {
        Self {
            raw: core::ptr::null_mut(),
            host,
            current_len: 0,
        }
    }

    /// Returns `true` if the underlying [`BamIter`] pointer is non-null.
    #[inline]
    pub fn is_valid(&self) -> bool {
        !self.raw.is_null()
    }

    /// Returns the raw `*mut BamIter` handle.
    #[inline]
    pub const fn as_raw(&self) -> *mut BamIter {
        self.raw
    }

    /// Returns the cached [`Host`] table.
    #[inline]
    pub const fn host(&self) -> &'a Host {
        self.host
    }

    /// Returns the number of entity rows in the currently active chunk (`0` before `next_chunk` or
    /// after exhaustion).
    #[inline]
    pub const fn current_len(&self) -> usize {
        self.current_len
    }

    /// Advances to the next non-empty chunk, returning `Some(chunk_len)` while chunks remain or
    /// `None` when exhausted.
    #[inline]
    pub fn next_chunk(&mut self) -> Option<usize> {
        if self.raw.is_null() {
            self.current_len = 0;
            return None;
        }
        let Some(next_fn) = self.host.api().iter_next_chunk else {
            self.current_len = 0;
            return None;
        };
        let mut len: usize = 0;
        // SAFETY: `self.raw` is a valid active `BamIter` for `'a` and `len` is writable stack storage.
        let has_more = unsafe { next_fn(self.raw, &mut len) };
        if has_more.get() {
            self.current_len = len;
            Some(len)
        } else {
            self.current_len = 0;
            None
        }
    }

    /// Resets the iterator back to its initial chunk so nested query loops can re-traverse.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        self.current_len = 0;
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Some(reset_fn) = self.host.api().iter_reset else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is a valid active `BamIter` for `'a`.
        unsafe { reset_fn(self.raw) }
    }

    /// Queries layout, UUID, access mode, and storage flags for `term_index`.
    #[inline]
    pub fn column_info(&self, term_index: usize) -> Result<BamColumnInfo, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        let Some(info_fn) = self.host.api().iter_column_info else {
            return Err(FfiResult::NULL_POINTER);
        };
        let mut out = MaybeUninit::<BamColumnInfo>::zeroed();
        // SAFETY: `self.raw` is valid for `'a` and `out` provides writable `BamColumnInfo` storage.
        let status = unsafe { info_fn(self.raw.cast_const(), term_index, out.as_mut_ptr()) };
        if status.is_ok() {
            // SAFETY: Host reported `FfiResult::OK`, confirming `*out` was initialized.
            Ok(unsafe { out.assume_init() })
        } else {
            Err(status)
        }
    }

    /// Borrows the `[FfiEntity; current_len]` slice for the active chunk (`&[]` if empty or unavailable).
    #[inline]
    pub fn entities(&self) -> &[FfiEntity] {
        if self.raw.is_null() || self.current_len == 0 {
            return &[];
        }
        let Some(entities_fn) = self.host.api().iter_entities else {
            return &[];
        };
        // SAFETY: `self.raw` is positioned at an active chunk of `self.current_len` rows.
        let ptr = unsafe { entities_fn(self.raw.cast_const()) };
        if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<FfiEntity>()) {
            return &[];
        }
        let Some(byte_len) = self
            .current_len
            .checked_mul(core::mem::size_of::<FfiEntity>())
        else {
            return &[];
        };
        if byte_len > isize::MAX as usize {
            return &[];
        }
        // SAFETY: Verified non-null, aligned `FfiEntity` pointer covering `self.current_len` entities
        // valid until the next `next_chunk`/`reset` mutation of `self`.
        unsafe { core::slice::from_raw_parts(ptr, self.current_len) }
    }

    /// Raw read-only byte pointer for dense column `term_index` in the active chunk (or null).
    #[inline]
    pub fn column_readonly_ptr(&self, term_index: usize) -> *const u8 {
        if self.raw.is_null() || self.current_len == 0 {
            return core::ptr::null();
        }
        let Some(col_fn) = self.host.api().iter_column_readonly else {
            return core::ptr::null();
        };
        // SAFETY: `self.raw` is an active `BamIter` positioned at a valid chunk.
        unsafe { col_fn(self.raw.cast_const(), term_index) }
    }

    /// Raw mutable byte pointer for dense column `term_index` (marks chunk ticks changed; or null).
    #[inline]
    pub fn column_mutable_ptr(&mut self, term_index: usize) -> *mut u8 {
        if self.raw.is_null() || self.current_len == 0 {
            return core::ptr::null_mut();
        }
        let Some(col_fn) = self.host.api().iter_column_mutable else {
            return core::ptr::null_mut();
        };
        // SAFETY: `self.raw` is an active `BamIter` and `&mut self` prevents concurrent access.
        unsafe { col_fn(self.raw, term_index) }
    }

    /// Raw mutable byte pointer for dense column `term_index` without marking change ticks.
    #[inline]
    pub fn column_mutable_untracked_ptr(&mut self, term_index: usize) -> *mut u8 {
        if self.raw.is_null() || self.current_len == 0 {
            return core::ptr::null_mut();
        }
        let Some(col_fn) = self.host.api().iter_column_mutable_untracked else {
            return core::ptr::null_mut();
        };
        // SAFETY: `self.raw` is an active `BamIter` and `&mut self` prevents concurrent access.
        unsafe { col_fn(self.raw, term_index) }
    }

    /// Marks rows `start_row..start_row + row_count` in writable column `term_index` as changed.
    #[inline]
    pub fn mark_changed_range(
        &mut self,
        term_index: usize,
        start_row: usize,
        row_count: usize,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        let Some(end_row) = start_row.checked_add(row_count) else {
            return FfiResult::OUT_OF_RANGE;
        };
        if end_row > self.current_len {
            return FfiResult::OUT_OF_RANGE;
        }
        let Some(mark_fn) = self.host.api().iter_mark_changed_range else {
            return FfiResult::NULL_POINTER;
        };
        // SAFETY: `self.raw` is active and `start_row..end_row` was bounds-checked against `current_len`.
        unsafe { mark_fn(self.raw, term_index, start_row, row_count) }
    }

    /// Marks a single row `row` in writable column `term_index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, term_index: usize, row: usize) -> FfiResult {
        self.mark_changed_range(term_index, row, 1)
    }

    /// Returns a [`ValidatedSlice`] stamped with [`BamColumnInfo`] for dense read column `term_index`.
    #[inline]
    pub fn readonly_slice(&self, term_index: usize) -> ValidatedSlice<'_> {
        let Ok(info) = self.column_info(term_index) else {
            return ValidatedSlice::invalid();
        };
        if !info.mode.has_entity_column_data() || info.is_resource() || info.is_sparse_set() {
            return ValidatedSlice::invalid();
        }
        let ptr = self.column_readonly_ptr(term_index);
        // SAFETY: `ptr` comes from `iter_column_readonly` for `self.current_len` rows matching `info`.
        unsafe { ValidatedSlice::from_raw_parts_with_info(ptr, self.current_len, info) }
            .with_manifest_name(self.host.manifest_name())
    }

    /// Returns a [`ValidatedSliceMut`] (marking chunk ticks changed) stamped with [`BamColumnInfo`].
    #[inline]
    pub fn mutable_slice(&mut self, term_index: usize) -> ValidatedSliceMut<'_> {
        let Ok(info) = self.column_info(term_index) else {
            return ValidatedSliceMut::invalid();
        };
        if !info.mode.is_mutable()
            || !info.mode.has_entity_column_data()
            || info.is_immutable()
            || info.is_resource()
            || info.is_sparse_set()
        {
            return ValidatedSliceMut::invalid();
        }
        let len = self.current_len;
        let ptr = self.column_mutable_ptr(term_index);
        let manifest_name = self.host.manifest_name();
        // SAFETY: `ptr` comes from `iter_column_mutable` for `len` rows under `&mut self`.
        unsafe { ValidatedSliceMut::from_raw_parts_with_info(ptr, len, info) }
            .with_manifest_name(manifest_name)
    }

    /// Returns an untracked [`ValidatedSliceMut`] (without marking chunk ticks changed) stamped
    /// with [`BamColumnInfo`].
    #[inline]
    pub fn mutable_slice_untracked(&mut self, term_index: usize) -> ValidatedSliceMut<'_> {
        let Ok(info) = self.column_info(term_index) else {
            return ValidatedSliceMut::invalid();
        };
        if !info.mode.is_mutable()
            || !info.mode.has_entity_column_data()
            || info.is_immutable()
            || info.is_resource()
            || info.is_sparse_set()
        {
            return ValidatedSliceMut::invalid();
        }
        let len = self.current_len;
        let ptr = self.column_mutable_untracked_ptr(term_index);
        let manifest_name = self.host.manifest_name();
        // SAFETY: `ptr` comes from `iter_column_mutable_untracked` for `len` rows under `&mut self`.
        unsafe { ValidatedSliceMut::from_raw_parts_with_info(ptr, len, info) }
            .with_manifest_name(manifest_name)
    }

    /// Borrows dense column `term_index` as `&[T]` after validating [`BamColumnInfo`] and optional
    /// `expected_uuid`.
    #[inline]
    pub fn column_checked<T: AbiPod>(
        &self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&[T]> {
        let info = self.column_info(term_index).ok()?;
        if !validate_dense_read_info::<T>(&info, expected_uuid) {
            return None;
        }
        if self.current_len == 0 {
            if info.mode.is_optional() {
                return None;
            }
            return Some(&[]);
        }
        let ptr = self.column_readonly_ptr(term_index);
        // SAFETY: `validate_dense_read_info` confirmed layout/UUID/mode and `ptr` covers `current_len` rows.
        unsafe { raw_dense_slice::<T>(ptr, self.current_len) }
    }

    /// Borrows dense column `term_index` as `&mut [T]` (marking chunk ticks changed) after
    /// validating [`BamColumnInfo`] and optional `expected_uuid`.
    #[inline]
    pub fn column_mut_checked<T: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&mut [T]> {
        let info = self.column_info(term_index).ok()?;
        if !validate_dense_write_info::<T>(&info, expected_uuid) {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if info.mode.is_optional() {
                return None;
            }
            return Some(&mut []);
        }
        let ptr = self.column_mutable_ptr(term_index);
        // SAFETY: `validate_dense_write_info` confirmed writable dense `T` column under `&mut self`.
        unsafe { raw_dense_slice_mut::<T>(ptr, len) }
    }

    /// Borrows dense column `term_index` as `&mut [T]` **without** automatically marking change ticks,
    /// after validating [`BamColumnInfo`] and optional `expected_uuid`.
    #[inline]
    pub fn column_mut_untracked_checked<T: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&mut [T]> {
        let info = self.column_info(term_index).ok()?;
        if !validate_dense_write_info::<T>(&info, expected_uuid) {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if info.mode.is_optional() {
                return None;
            }
            return Some(&mut []);
        }
        let ptr = self.column_mutable_untracked_ptr(term_index);
        // SAFETY: `validate_dense_write_info` confirmed writable dense `T` column under `&mut self`.
        unsafe { raw_dense_slice_mut::<T>(ptr, len) }
    }

    /// Simultaneously borrows one read-only dense column (`read_col`) and one mutable dense column
    /// (`write_col`), rejecting identical term indices, identical component handles, or overlapping
    /// byte ranges.
    #[inline]
    pub fn read_write_checked<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        read_uuid: Option<UuidFilter>,
        write_col: usize,
        write_uuid: Option<UuidFilter>,
    ) -> Option<(&[R], &mut [W])> {
        if read_col == write_col {
            return None;
        }
        let r_info = self.column_info(read_col).ok()?;
        let w_info = self.column_info(write_col).ok()?;
        if r_info.component == w_info.component {
            return None;
        }
        if !validate_dense_read_info::<R>(&r_info, read_uuid)
            || !validate_dense_write_info::<W>(&w_info, write_uuid)
        {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if r_info.mode.is_optional() || w_info.mode.is_optional() {
                return None;
            }
            return Some((&[], &mut []));
        }
        let r_ptr = self.column_readonly_ptr(read_col);
        let w_ptr = self.column_mutable_ptr(write_col);
        let r_size = core::mem::size_of::<R>();
        let w_size = core::mem::size_of::<W>();
        if r_size > 0 && w_size > 0 {
            if r_ptr.is_null() || w_ptr.is_null() {
                return None;
            }
            let r_bytes = len.checked_mul(r_size)?;
            let w_bytes = len.checked_mul(w_size)?;
            let r_start = r_ptr as usize;
            let w_start = w_ptr as usize;
            let r_end = r_start.checked_add(r_bytes)?;
            let w_end = w_start.checked_add(w_bytes)?;
            if r_start < w_end && w_start < r_end {
                return None;
            }
        }
        // SAFETY: Verified distinct columns, distinct `BamComponent` handles, non-overlapping byte
        // ranges, and matching `BamColumnInfo` layouts under `&mut self`.
        let r_slice = unsafe { raw_dense_slice::<R>(r_ptr, len) }?;
        // SAFETY: Disjoint from `r_slice` and exclusively borrowed for `&mut self`.
        let w_slice = unsafe { raw_dense_slice_mut::<W>(w_ptr, len) }?;
        Some((r_slice, w_slice))
    }

    /// Simultaneously borrows two disjoint mutable dense columns (`col_a` and `col_b`).
    #[inline]
    pub fn write_2_checked<W1: AbiPod, W2: AbiPod>(
        &mut self,
        col_a: usize,
        uuid_a: Option<UuidFilter>,
        col_b: usize,
        uuid_b: Option<UuidFilter>,
    ) -> Option<(&mut [W1], &mut [W2])> {
        if col_a == col_b {
            return None;
        }
        let info_a = self.column_info(col_a).ok()?;
        let info_b = self.column_info(col_b).ok()?;
        if info_a.component == info_b.component {
            return None;
        }
        if !validate_dense_write_info::<W1>(&info_a, uuid_a)
            || !validate_dense_write_info::<W2>(&info_b, uuid_b)
        {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if info_a.mode.is_optional() || info_b.mode.is_optional() {
                return None;
            }
            return Some((&mut [], &mut []));
        }
        let ptr_a = self.column_mutable_ptr(col_a);
        let ptr_b = self.column_mutable_ptr(col_b);
        let size_a = core::mem::size_of::<W1>();
        let size_b = core::mem::size_of::<W2>();
        if size_a > 0 && size_b > 0 {
            if ptr_a.is_null() || ptr_b.is_null() {
                return None;
            }
            let bytes_a = len.checked_mul(size_a)?;
            let bytes_b = len.checked_mul(size_b)?;
            let start_a = ptr_a as usize;
            let start_b = ptr_b as usize;
            let end_a = start_a.checked_add(bytes_a)?;
            let end_b = start_b.checked_add(bytes_b)?;
            if start_a < end_b && start_b < end_a {
                return None;
            }
        }
        // SAFETY: Verified distinct columns, distinct `BamComponent` handles, and non-overlapping
        // byte ranges under `&mut self`.
        let slice_a = unsafe { raw_dense_slice_mut::<W1>(ptr_a, len) }?;
        // SAFETY: Disjoint from `slice_a` and exclusively borrowed for `&mut self`.
        let slice_b = unsafe { raw_dense_slice_mut::<W2>(ptr_b, len) }?;
        Some((slice_a, slice_b))
    }

    /// Simultaneously borrows `[FfiEntity; current_len]` and mutable dense column `write_col`,
    /// verifying disjointness between the entity array and component column bytes.
    #[inline]
    pub fn entities_and_column_mut_checked<W: AbiPod>(
        &mut self,
        write_col: usize,
        write_uuid: Option<UuidFilter>,
    ) -> Option<(&[FfiEntity], &mut [W])> {
        let w_info = self.column_info(write_col).ok()?;
        if !validate_dense_write_info::<W>(&w_info, write_uuid) {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if w_info.mode.is_optional() {
                return None;
            }
            return Some((&[], &mut []));
        }
        let entities_fn = self.host.api().iter_entities?;
        // SAFETY: `self.raw` is active at a chunk of `len` rows.
        let e_ptr = unsafe { entities_fn(self.raw.cast_const()) };
        if e_ptr.is_null() || !(e_ptr as usize).is_multiple_of(core::mem::align_of::<FfiEntity>()) {
            return None;
        }
        let e_bytes = len.checked_mul(core::mem::size_of::<FfiEntity>())?;
        if e_bytes > isize::MAX as usize {
            return None;
        }
        let w_ptr = self.column_mutable_ptr(write_col);
        let w_size = core::mem::size_of::<W>();
        if w_size > 0 {
            if w_ptr.is_null() {
                return None;
            }
            let w_bytes = len.checked_mul(w_size)?;
            let e_start = e_ptr as usize;
            let w_start = w_ptr as usize;
            let e_end = e_start.checked_add(e_bytes)?;
            let w_end = w_start.checked_add(w_bytes)?;
            if e_start < w_end && w_start < e_end {
                return None;
            }
        }
        // SAFETY: Entity slice and mutable component column were verified non-overlapping.
        let entities = unsafe { core::slice::from_raw_parts(e_ptr, len) };
        // SAFETY: Disjoint from `entities` and exclusively borrowed under `&mut self`.
        let w_slice = unsafe { raw_dense_slice_mut::<W>(w_ptr, len) }?;
        Some((entities, w_slice))
    }

    /// Simultaneously borrows `[FfiEntity; current_len]`, read-only dense column `read_col`, and
    /// disjoint mutable dense column `write_col`.
    #[inline]
    pub fn entities_and_read_write_checked<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        read_uuid: Option<UuidFilter>,
        write_col: usize,
        write_uuid: Option<UuidFilter>,
    ) -> Option<(&[FfiEntity], &[R], &mut [W])> {
        if read_col == write_col {
            return None;
        }
        let r_info = self.column_info(read_col).ok()?;
        let w_info = self.column_info(write_col).ok()?;
        if r_info.component == w_info.component {
            return None;
        }
        if !validate_dense_read_info::<R>(&r_info, read_uuid)
            || !validate_dense_write_info::<W>(&w_info, write_uuid)
        {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            if r_info.mode.is_optional() || w_info.mode.is_optional() {
                return None;
            }
            return Some((&[], &[], &mut []));
        }
        let entities_fn = self.host.api().iter_entities?;
        // SAFETY: `self.raw` is active at a chunk of `len` rows.
        let e_ptr = unsafe { entities_fn(self.raw.cast_const()) };
        if e_ptr.is_null() || !(e_ptr as usize).is_multiple_of(core::mem::align_of::<FfiEntity>()) {
            return None;
        }
        let e_bytes = len.checked_mul(core::mem::size_of::<FfiEntity>())?;
        if e_bytes > isize::MAX as usize {
            return None;
        }
        let r_ptr = self.column_readonly_ptr(read_col);
        let w_ptr = self.column_mutable_ptr(write_col);
        let r_size = core::mem::size_of::<R>();
        let w_size = core::mem::size_of::<W>();
        if w_size > 0 {
            if w_ptr.is_null() {
                return None;
            }
            let w_bytes = len.checked_mul(w_size)?;
            let w_start = w_ptr as usize;
            let w_end = w_start.checked_add(w_bytes)?;
            let e_start = e_ptr as usize;
            let e_end = e_start.checked_add(e_bytes)?;
            if e_start < w_end && w_start < e_end {
                return None;
            }
            if r_size > 0 {
                if r_ptr.is_null() {
                    return None;
                }
                let r_bytes = len.checked_mul(r_size)?;
                let r_start = r_ptr as usize;
                let r_end = r_start.checked_add(r_bytes)?;
                if r_start < w_end && w_start < r_end {
                    return None;
                }
            }
        }
        // SAFETY: Verified non-overlapping with `w_ptr` and aligned for `FfiEntity`.
        let entities = unsafe { core::slice::from_raw_parts(e_ptr, len) };
        // SAFETY: Verified non-overlapping with `w_ptr` and matching `BamColumnInfo`.
        let r_slice = unsafe { raw_dense_slice::<R>(r_ptr, len) }?;
        // SAFETY: Disjoint from `entities` and `r_slice` under `&mut self`.
        let w_slice = unsafe { raw_dense_slice_mut::<W>(w_ptr, len) }?;
        Some((entities, r_slice, w_slice))
    }

    /// Materializes indirect read-only sparse-set row pointers `[ *const u8; current_len ]` and
    /// wraps them in a validated [`SparseRows`] view.
    #[inline]
    pub fn rows_checked<T: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<SparseRows<'_, T>> {
        let info = self.column_info(term_index).ok()?;
        if !validate_rows_read_info::<T>(&info, expected_uuid) {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            // SAFETY: Zero-length `SparseRows` does not dereference its pointer.
            return unsafe { SparseRows::from_raw_parts(core::ptr::null(), 0, true) };
        }
        let rows_fn = self.host.api().iter_column_rows_readonly?;
        // SAFETY: `self.raw` is active at a chunk of `len` rows; `&mut self` keeps the host's
        // scratch row-pointer array alive and unaliased for the returned `SparseRows<'_, T>`.
        let ptrs = unsafe { rows_fn(self.raw, term_index) };
        if ptrs.is_null() {
            return None;
        }
        // SAFETY: `ptrs` covers `len` row pointers valid until `self` is next used.
        unsafe { SparseRows::from_raw_parts(ptrs, len, info.mode.is_optional()) }
    }

    /// Materializes indirect mutable sparse-set row pointers `[ *mut u8; current_len ]` and wraps
    /// them in a validated [`SparseRowsMut`] view.
    #[inline]
    pub fn rows_mut_checked<T: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<SparseRowsMut<'_, T>> {
        let info = self.column_info(term_index).ok()?;
        if !validate_rows_write_info::<T>(&info, expected_uuid) {
            return None;
        }
        let len = self.current_len;
        if len == 0 {
            // SAFETY: Zero-length `SparseRowsMut` does not dereference its pointer.
            return unsafe { SparseRowsMut::from_raw_parts(core::ptr::null(), 0, true) };
        }
        let rows_fn = self.host.api().iter_column_rows_mutable?;
        // SAFETY: `self.raw` is active at a chunk of `len` rows; `&mut self` keeps the host's
        // scratch row-pointer array and component borrows exclusive for `'self`.
        let ptrs = unsafe { rows_fn(self.raw, term_index) };
        if ptrs.is_null() {
            return None;
        }
        // SAFETY: `ptrs` covers `len` writable row pointers valid until `self` is next used.
        unsafe { SparseRowsMut::from_raw_parts(ptrs, len, info.mode.is_optional()) }
    }

    /// Returns the raw read-only singleton resource pointer at `term_index` (or null).
    #[inline]
    pub fn res_ptr(&self, term_index: usize) -> *const u8 {
        if self.raw.is_null() {
            return core::ptr::null();
        }
        let Some(res_fn) = self.host.api().res_ptr else {
            return core::ptr::null();
        };
        // SAFETY: `self.raw` is a valid active `BamIter` for `'a`.
        unsafe { res_fn(self.raw.cast_const(), term_index) }
    }

    /// Returns the raw mutable singleton resource pointer at `term_index` (or null).
    #[inline]
    pub fn res_mut_ptr(&mut self, term_index: usize) -> *mut u8 {
        if self.raw.is_null() {
            return core::ptr::null_mut();
        }
        let Some(res_mut_fn) = self.host.api().res_mut_ptr else {
            return core::ptr::null_mut();
        };
        // SAFETY: `self.raw` is an active `BamIter` and `&mut self` prevents concurrent access.
        unsafe { res_mut_fn(self.raw, term_index) }
    }

    /// Borrows singleton resource `R` declared at `term_index` after validating [`BamColumnInfo`]
    /// and optional `expected_uuid`.
    #[inline]
    pub fn resource_checked<R: AbiPod>(
        &self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&R> {
        let info = self.column_info(term_index).ok()?;
        if !validate_resource_read_info::<R>(&info, expected_uuid) {
            return None;
        }
        let ptr = self.res_ptr(term_index);
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<R>() == 0 {
            // SAFETY: Zero-sized `R: AbiPod` may be referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<R>::dangling().as_ref() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
            return None;
        }
        // SAFETY: `validate_resource_read_info` confirmed `R`'s size, alignment, resource mode, and
        // optional UUID; `ptr` is non-null, aligned, and valid for `&self`.
        Some(unsafe { &*ptr.cast::<R>() })
    }

    /// Mutably borrows singleton resource `R` declared with `RESOURCE_WRITE` at `term_index` after
    /// validating [`BamColumnInfo`] and optional `expected_uuid`.
    #[inline]
    pub fn resource_mut_checked<R: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Option<&mut R> {
        let info = self.column_info(term_index).ok()?;
        if !validate_resource_write_info::<R>(&info, expected_uuid) {
            return None;
        }
        let ptr = self.res_mut_ptr(term_index);
        if ptr.is_null() {
            return None;
        }
        if core::mem::size_of::<R>() == 0 {
            // SAFETY: Zero-sized `R: AbiPod` may be mutably referenced via `NonNull::dangling()`.
            return Some(unsafe { core::ptr::NonNull::<R>::dangling().as_mut() });
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
            return None;
        }
        // SAFETY: `validate_resource_write_info` confirmed `RESOURCE_WRITE`, non-immutable, layout,
        // and optional UUID; `&mut self` guarantees exclusive access.
        Some(unsafe { &mut *ptr.cast::<R>() })
    }

    /// Executes `f` in parallel across disjoint sub-range chunks on the host `ComputeTaskPool`
    /// (`P-07`), catching panics per task so unwinding never crosses `extern "C"`.
    pub fn par_for_each_chunk<F>(&mut self, f: F) -> FfiResult
    where
        F: Fn(&mut ValidatedIter<'_>) -> FfiResult + Sync,
    {
        if self.raw.is_null() {
            return FfiResult::OK;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, par_for_each_chunk),
            core::mem::size_of::<Option<BamParForEachChunkFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(par_fn) = self.host.api().par_for_each_chunk else {
            return FfiResult::UNSUPPORTED;
        };
        self.current_len = 0;
        let task_ctx = ParChunkTaskContext {
            host: self.host,
            func: f,
        };
        // SAFETY: `self.raw` is an active top-level `BamIter`, `task_ctx` stays live on the stack
        // across the synchronous `par_fn` scope, `F: Sync` permits concurrent calls across worker
        // threads, and `par_chunk_task_trampoline` catches any panic on the guest heap.
        unsafe {
            par_fn(
                self.raw,
                Some(par_chunk_task_trampoline::<F>),
                (&raw const task_ctx).cast_mut().cast::<c_void>(),
            )
        }
    }

    /// Reads up to `max_count` unread POD messages from `term_index` (`MESSAGE_READ` or
    /// `MESSAGE_WRITE`), advancing the running system's per-term message cursor (`F-02`).
    pub fn read_messages_batch_checked<M: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
        max_count: usize,
    ) -> Result<Vec<M>, FfiResult> {
        if self.raw.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, msg_read_batch),
            core::mem::size_of::<Option<BamMsgReadBatchFn>>(),
        ) {
            return Err(FfiResult::UNSUPPORTED);
        }
        let Some(read_fn) = self.host.api().msg_read_batch else {
            return Err(FfiResult::UNSUPPORTED);
        };
        let info = self.column_info(term_index)?;
        if !validate_message_read_info::<M>(&info, expected_uuid) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        if max_count == 0 {
            return Ok(Vec::new());
        }
        let mut scratch: Vec<MaybeUninit<M>> = Vec::with_capacity(max_count);
        let out_ptr = if core::mem::size_of::<M>() == 0 {
            core::ptr::NonNull::<u8>::dangling().as_ptr()
        } else {
            scratch.as_mut_ptr().cast::<u8>()
        };
        let mut read_count = 0usize;
        // SAFETY: `self.raw` is active, `validate_message_read_info` confirmed element size and
        // alignment match `M`, and `scratch` has capacity for `max_count` elements of `M`.
        let status = unsafe { read_fn(self.raw, term_index, out_ptr, max_count, &mut read_count) };
        if !status.is_ok() {
            return Err(status);
        }
        let actual = read_count.min(max_count);
        // SAFETY: Host copied `actual` initialized elements of `M` into `scratch[..actual]`.
        unsafe {
            scratch.set_len(actual);
        }
        let mut out = Vec::with_capacity(actual);
        for slot in scratch {
            // SAFETY: Every element in `scratch[..actual]` was initialized by `msg_read_batch`.
            out.push(unsafe { slot.assume_init() });
        }
        Ok(out)
    }

    /// Drains all currently unread POD messages from `term_index` (`F-02`).
    pub fn drain_messages_checked<M: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<Vec<M>, FfiResult> {
        const BATCH_SIZE: usize = 64;
        let mut all = Vec::new();
        loop {
            let batch =
                self.read_messages_batch_checked::<M>(term_index, expected_uuid, BATCH_SIZE)?;
            let count = batch.len();
            all.extend(batch);
            if count < BATCH_SIZE {
                break;
            }
        }
        Ok(all)
    }

    /// Appends a POD message `msg` to the Bevy `Messages<M>` stream declared with `MESSAGE_WRITE`
    /// at `term_index` (`F-02`).
    pub fn send_message_checked<M: AbiPod>(
        &mut self,
        term_index: usize,
        expected_uuid: Option<UuidFilter>,
        msg: &M,
    ) -> FfiResult {
        if self.raw.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.host.api().covers_slot(
            core::mem::offset_of!(BamHostApi, msg_send),
            core::mem::size_of::<Option<BamMsgSendFn>>(),
        ) {
            return FfiResult::UNSUPPORTED;
        }
        let Some(send_fn) = self.host.api().msg_send else {
            return FfiResult::UNSUPPORTED;
        };
        let info = match self.column_info(term_index) {
            Ok(i) => i,
            Err(err) => return err,
        };
        if !validate_message_write_info::<M>(&info, expected_uuid) {
            return FfiResult::TYPE_MISMATCH;
        }
        let data_ptr = if core::mem::size_of::<M>() == 0 {
            core::ptr::NonNull::<u8>::dangling().as_ptr().cast_const()
        } else {
            core::ptr::from_ref(msg).cast::<u8>()
        };
        // SAFETY: `self.raw` is active, `validate_message_write_info` verified `M`'s exact layout
        // and `MESSAGE_WRITE` permission, and `data_ptr` points to `size_of::<M>()` readable bytes.
        unsafe { send_fn(self.raw, term_index, data_ptr, core::mem::size_of::<M>()) }
    }
}

struct ParChunkTaskContext<'h, F> {
    host: &'h Host,
    func: F,
}

unsafe extern "C" fn par_chunk_task_trampoline<F>(
    sub_iter: *mut BamIter,
    user_data: *mut c_void,
) -> FfiResult
where
    F: Fn(&mut ValidatedIter<'_>) -> FfiResult + Sync,
{
    catch_guest_result(|| {
        if sub_iter.is_null() || user_data.is_null() {
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `user_data` points to `ParChunkTaskContext<'_, F>` live on the caller's stack
        // throughout `par_for_each_chunk`, and `F: Sync` allows concurrent shared reads.
        let ctx = unsafe { &*user_data.cast::<ParChunkTaskContext<'_, F>>() };
        // SAFETY: `sub_iter` is an isolated sub-range `BamIter` valid on this worker thread for
        // the duration of this task callback.
        let mut task_iter = unsafe { ValidatedIter::from_raw(sub_iter, ctx.host) };
        while task_iter.next_chunk().is_some() {
            let status = (ctx.func)(&mut task_iter);
            if !status.is_ok() {
                return status;
            }
        }
        FfiResult::OK
    })
}

#[inline]
fn validate_message_read_info<M: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if !matches!(
        info.mode,
        AccessMode::MESSAGE_READ | AccessMode::MESSAGE_WRITE
    ) || !info.is_message()
        || info.is_resource()
    {
        return false;
    }
    if info.size != core::mem::size_of::<M>() || info.align != core::mem::align_of::<M>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[inline]
fn validate_message_write_info<M: AbiPod>(
    info: &BamColumnInfo,
    expected_uuid: Option<UuidFilter>,
) -> bool {
    if info.mode != AccessMode::MESSAGE_WRITE
        || info.is_immutable()
        || !info.is_message()
        || info.is_resource()
    {
        return false;
    }
    if info.size != core::mem::size_of::<M>() || info.align != core::mem::align_of::<M>() {
        return false;
    }
    if let Some(filter) = expected_uuid
        && !filter.matches(info.uuid)
    {
        return false;
    }
    true
}

#[derive(Clone, Copy)]
struct DispensedBorrow {
    component: BamComponent,
    start: usize,
    end: usize,
    is_mut: bool,
}

/// Sequential per-chunk column dispenser verifying disjoint borrows across up to
/// [`MAX_TERMS_PER_QUERY`] tuple terms (`G-03`) so safe `bevy_abi_mod_guest` tuple queries require
/// zero `unsafe` code.
pub struct ChunkColumnDispenser<'chunk, 'ctx> {
    iter: &'chunk mut ValidatedIter<'ctx>,
    next_term_index: usize,
    borrowed: [DispensedBorrow; MAX_TERMS_PER_QUERY],
    borrowed_count: usize,
    entities_range: Option<(usize, usize)>,
}

impl<'chunk, 'ctx> ChunkColumnDispenser<'chunk, 'ctx> {
    /// Creates a column dispenser over the currently active chunk of `iter`.
    #[inline]
    pub fn new(iter: &'chunk mut ValidatedIter<'ctx>) -> Self {
        Self {
            iter,
            next_term_index: 0,
            borrowed: [DispensedBorrow {
                component: BamComponent::INVALID,
                start: 0,
                end: 0,
                is_mut: false,
            }; MAX_TERMS_PER_QUERY],
            borrowed_count: 0,
            entities_range: None,
        }
    }

    /// Returns the cached [`Host`] table for this iterator.
    #[inline]
    pub const fn host(&self) -> &'ctx Host {
        self.iter.host()
    }

    /// Returns the number of entity rows in the active chunk.
    #[inline]
    pub const fn len(&self) -> usize {
        self.iter.current_len()
    }

    /// Returns `true` if `len() == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn check_and_record_borrow(
        &mut self,
        component: BamComponent,
        start: usize,
        byte_len: usize,
        is_mut: bool,
    ) -> Result<(), FfiResult> {
        let end = start.checked_add(byte_len).ok_or(FfiResult::OUT_OF_RANGE)?;
        if is_mut
            && byte_len > 0
            && let Some((ent_start, ent_end)) = self.entities_range
            && ent_end > ent_start
            && start < ent_end
            && ent_start < end
        {
            return Err(FfiResult::ACCESS_CONFLICT);
        }
        for prev in &self.borrowed[..self.borrowed_count] {
            if is_mut || prev.is_mut {
                if prev.component == component {
                    return Err(FfiResult::ACCESS_CONFLICT);
                }
                let prev_non_empty = prev.end > prev.start;
                if byte_len > 0 && prev_non_empty && start < prev.end && prev.start < end {
                    return Err(FfiResult::ACCESS_CONFLICT);
                }
            }
        }
        if self.borrowed_count >= MAX_TERMS_PER_QUERY {
            return Err(FfiResult::CAPACITY_EXCEEDED);
        }
        self.borrowed[self.borrowed_count] = DispensedBorrow {
            component,
            start,
            end,
            is_mut,
        };
        self.borrowed_count += 1;
        Ok(())
    }

    /// Dispenses the contiguous `[FfiEntity; len]` slice for the active chunk without consuming a
    /// `BamTermDesc` index (`G-03`).
    pub fn entities(&mut self) -> Result<&'chunk [FfiEntity], FfiResult> {
        let len = self.iter.current_len();
        if len == 0 {
            return Ok(&[]);
        }
        let ent_fn = self
            .iter
            .host
            .api()
            .iter_entities
            .ok_or(FfiResult::NULL_POINTER)?;
        // SAFETY: `self.iter.raw` is active at a chunk of `len` entities for `'chunk`.
        let ptr = unsafe { ent_fn(self.iter.raw.cast_const()) };
        if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<FfiEntity>()) {
            return Err(FfiResult::NULL_POINTER);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<FfiEntity>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        let start = ptr as usize;
        let end = start.checked_add(byte_len).ok_or(FfiResult::OUT_OF_RANGE)?;
        for prev in &self.borrowed[..self.borrowed_count] {
            if prev.is_mut && prev.end > prev.start && start < prev.end && prev.start < end {
                return Err(FfiResult::ACCESS_CONFLICT);
            }
        }
        self.entities_range = Some((start, end));
        // SAFETY: `ptr` is non-null, aligned, disjoint from all mutable column borrows, and valid
        // for `len` `FfiEntity` elements throughout `'chunk`.
        Ok(unsafe { core::slice::from_raw_parts(ptr, len) })
    }

    /// Dispenses the next dense read-only `&'chunk [T]` column (`READ`, `CHANGED`, or `ADDED`).
    pub fn next_read<T: AbiPod>(
        &mut self,
        expected_mode: AccessMode,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<&'chunk [T], FfiResult> {
        let term_index = self.next_term_index;
        self.next_term_index += 1;
        let info = self.iter.column_info(term_index)?;
        if info.mode != expected_mode || !validate_dense_read_info::<T>(&info, expected_uuid) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let len = self.iter.current_len();
        if len == 0 || core::mem::size_of::<T>() == 0 {
            self.check_and_record_borrow(info.component, 0, 0, false)?;
            // SAFETY: Zero-length or ZST slice uses `NonNull::dangling()` without dereferencing.
            return Ok(unsafe {
                core::slice::from_raw_parts(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
            });
        }
        let col_fn = self
            .iter
            .host
            .api()
            .iter_column_readonly
            .ok_or(FfiResult::NULL_POINTER)?;
        // SAFETY: `self.iter.raw` is active at a chunk of `len` rows for `'chunk`.
        let ptr = unsafe { col_fn(self.iter.raw.cast_const(), term_index) };
        if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        self.check_and_record_borrow(info.component, ptr as usize, byte_len, false)?;
        // SAFETY: `validate_dense_read_info` confirmed `T`'s layout and UUID; `check_and_record_borrow`
        // confirmed no mutable borrow aliases this component or byte range for `'chunk`.
        Ok(unsafe { core::slice::from_raw_parts(ptr.cast::<T>(), len) })
    }

    /// Dispenses the next dense mutable `&'chunk mut [T]` column (`WRITE`).
    pub fn next_write<T: AbiPod>(
        &mut self,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<&'chunk mut [T], FfiResult> {
        let term_index = self.next_term_index;
        self.next_term_index += 1;
        let info = self.iter.column_info(term_index)?;
        if info.mode != AccessMode::WRITE || !validate_dense_write_info::<T>(&info, expected_uuid) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let len = self.iter.current_len();
        if len == 0 || core::mem::size_of::<T>() == 0 {
            self.check_and_record_borrow(info.component, 0, 0, true)?;
            if len > 0
                && let Some(col_fn) = self.iter.host.api().iter_column_mutable
            {
                // Mark ZST change ticks via host `iter_column_mutable`.
                // SAFETY: `self.iter.raw` is active at a valid chunk.
                let _ = unsafe { col_fn(self.iter.raw, term_index) };
            }
            // SAFETY: Zero-length or ZST mutable slice over `NonNull::dangling()` with unique component check.
            return Ok(unsafe {
                core::slice::from_raw_parts_mut(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
            });
        }
        let col_fn = self
            .iter
            .host
            .api()
            .iter_column_mutable
            .ok_or(FfiResult::NULL_POINTER)?;
        // SAFETY: `self.iter.raw` is active at a chunk of `len` rows for `'chunk`.
        let ptr = unsafe { col_fn(self.iter.raw, term_index) };
        if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        self.check_and_record_borrow(info.component, ptr as usize, byte_len, true)?;
        // SAFETY: `validate_dense_write_info` verified `WRITE`, non-immutable, layout, and UUID;
        // `check_and_record_borrow` verified strict disjointness from all other borrows in `'chunk`.
        Ok(unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), len) })
    }

    /// Dispenses the next optional dense read-only `Option<&'chunk [T]>` column (`OPTIONAL_READ`).
    pub fn next_optional_read<T: AbiPod>(
        &mut self,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<Option<&'chunk [T]>, FfiResult> {
        let term_index = self.next_term_index;
        self.next_term_index += 1;
        let info = self.iter.column_info(term_index)?;
        if info.mode != AccessMode::OPTIONAL_READ
            || !validate_dense_read_info::<T>(&info, expected_uuid)
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let len = self.iter.current_len();
        let col_fn = self
            .iter
            .host
            .api()
            .iter_column_readonly
            .ok_or(FfiResult::NULL_POINTER)?;
        // SAFETY: `self.iter.raw` is active at a chunk of `len` rows for `'chunk`.
        let ptr = unsafe { col_fn(self.iter.raw.cast_const(), term_index) };
        if ptr.is_null() {
            return Ok(None);
        }
        if core::mem::size_of::<T>() == 0 {
            self.check_and_record_borrow(info.component, 0, 0, false)?;
            // SAFETY: Non-null optional ZST column is present for all `len` entities in the chunk.
            return Ok(Some(unsafe {
                core::slice::from_raw_parts(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
            }));
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        self.check_and_record_borrow(info.component, ptr as usize, byte_len, false)?;
        // SAFETY: Present optional dense column verified for layout, UUID, alignment, and non-aliasing.
        Ok(Some(unsafe {
            core::slice::from_raw_parts(ptr.cast::<T>(), len)
        }))
    }

    /// Dispenses the next optional dense mutable `Option<&'chunk mut [T]>` column (`OPTIONAL_WRITE`).
    pub fn next_optional_write<T: AbiPod>(
        &mut self,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<Option<&'chunk mut [T]>, FfiResult> {
        let term_index = self.next_term_index;
        self.next_term_index += 1;
        let info = self.iter.column_info(term_index)?;
        if info.mode != AccessMode::OPTIONAL_WRITE
            || !validate_dense_write_info::<T>(&info, expected_uuid)
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let len = self.iter.current_len();
        let col_fn = self
            .iter
            .host
            .api()
            .iter_column_mutable
            .ok_or(FfiResult::NULL_POINTER)?;
        // SAFETY: `self.iter.raw` is active at a chunk of `len` rows for `'chunk`.
        let ptr = unsafe { col_fn(self.iter.raw, term_index) };
        if ptr.is_null() {
            return Ok(None);
        }
        if core::mem::size_of::<T>() == 0 {
            self.check_and_record_borrow(info.component, 0, 0, true)?;
            // SAFETY: Present optional ZST mutable column verified disjoint by component handle.
            return Ok(Some(unsafe {
                core::slice::from_raw_parts_mut(core::ptr::NonNull::<T>::dangling().as_ptr(), len)
            }));
        }
        if !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        let byte_len = len
            .checked_mul(core::mem::size_of::<T>())
            .ok_or(FfiResult::OUT_OF_RANGE)?;
        self.check_and_record_borrow(info.component, ptr as usize, byte_len, true)?;
        // SAFETY: Present optional dense mutable column verified for layout, UUID, alignment, and disjointness.
        Ok(Some(unsafe {
            core::slice::from_raw_parts_mut(ptr.cast::<T>(), len)
        }))
    }

    /// Validates and advances past a pure filter term (`FILTER_ONLY` / `WITHOUT`) (`G-03`).
    pub fn next_filter<T: AbiPod>(
        &mut self,
        expected_mode: AccessMode,
        expected_uuid: Option<UuidFilter>,
    ) -> Result<(), FfiResult> {
        let term_index = self.next_term_index;
        self.next_term_index += 1;
        let info = self.iter.column_info(term_index)?;
        if info.mode != expected_mode || info.is_resource() || info.is_message() {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        if info.size != core::mem::size_of::<T>() || info.align != core::mem::align_of::<T>() {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        if let Some(filter) = expected_uuid
            && !filter.matches(info.uuid)
        {
            return Err(FfiResult::TYPE_MISMATCH);
        }
        Ok(())
    }
}
