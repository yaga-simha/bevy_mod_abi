//! Raw chunk iterator dispatchers, slice validation, and unforgeable validated iteration handles.

use core::ffi::CStr;
use core::marker::PhantomData;

use bevy_abi_mod_sys::{AbiPod, FfiEntity, FfiResult, HostApi, IterResetFn, RawChunkIter};

use crate::resolve::ResourceBridgeVTable;

/// Returns `true` if the two half-open byte ranges `[addr_a, addr_a + len_a)` and
/// `[addr_b, addr_b + len_b)` overlap.
#[inline]
pub fn byte_ranges_overlap(addr_a: usize, len_a: usize, addr_b: usize, len_b: usize) -> bool {
    if len_a == 0 || len_b == 0 {
        return false;
    }
    let end_a = addr_a.saturating_add(len_a);
    let end_b = addr_b.saturating_add(len_b);
    addr_a < end_b && addr_b < end_a
}

/// Validates and constructs an `&[FfiEntity]` slice from a raw entity column pointer and row count.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized [`FfiEntity`]
/// values valid for `'a`.
#[inline]
pub unsafe fn validated_entities_slice<'a>(ptr: *const FfiEntity, count: usize) -> &'a [FfiEntity] {
    if count == 0 || ptr.is_null() {
        return &[];
    }
    let elem_size = core::mem::size_of::<FfiEntity>();
    let elem_align = core::mem::align_of::<FfiEntity>();
    if !(ptr as usize).is_multiple_of(elem_align) || count > (isize::MAX as usize) {
        return &[];
    }
    let Some(total_bytes) = count.checked_mul(elem_size) else {
        return &[];
    };
    if total_bytes > (isize::MAX as usize) || (ptr as usize).checked_add(total_bytes).is_none() {
        return &[];
    }
    // SAFETY:
    // - `ptr` is non-null and aligned for `FfiEntity`.
    // - `total_bytes <= isize::MAX as usize` without address-space wrap-around.
    // - Caller ties `'a` to the active chunk borrow.
    unsafe { core::slice::from_raw_parts(ptr, count) }
}

/// Calls `reset_fn(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_reset(raw: *mut RawChunkIter, reset_fn: Option<IterResetFn>) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(reset_fn) = reset_fn else {
        return FfiResult::NULL_POINTER;
    };
    // SAFETY: `raw` is non-null and exclusively borrowed by the caller.
    unsafe { reset_fn(raw) }
}

/// Calls `api.iter_next_chunk(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_next_chunk(raw: *mut RawChunkIter, api: &HostApi) -> usize {
    if raw.is_null() {
        return 0;
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_next_chunk)(raw) }
}

/// Calls `api.iter_column_readonly(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_column_readonly(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
) -> *const u8 {
    if raw.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_column_readonly)(raw, index) }
}

/// Calls `api.iter_column_mutable(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_column_mutable(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
) -> *mut u8 {
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_column_mutable)(raw, index) }
}

/// Calls `api.iter_mark_changed(raw, index, row)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_mark_changed(
    raw: *mut RawChunkIter,
    api: &HostApi,
    index: usize,
    row: usize,
) -> FfiResult {
    if raw.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_mark_changed)(raw, index, row) }
}

/// Calls `api.iter_entities(raw)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_entities(raw: *mut RawChunkIter, api: &HostApi) -> *const FfiEntity {
    if raw.is_null() {
        return core::ptr::null();
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_entities)(raw) }
}

/// Calls `api.iter_column_stride(raw, index)` if `raw` is non-null.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_column_stride(raw: *mut RawChunkIter, api: &HostApi, index: usize) -> usize {
    if raw.is_null() {
        return 0;
    }
    // SAFETY: Delegating to `HostApi` function pointer with non-null `raw`.
    unsafe { (api.iter_column_stride)(raw, index) }
}

/// Resolves a read-only resource byte pointer from `raw` via `res_vtable`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_resource_readonly(
    raw: *mut RawChunkIter,
    res_vtable: &ResourceBridgeVTable,
    resource_name: &CStr,
) -> *const u8 {
    if raw.is_null() {
        return core::ptr::null();
    }
    let Some(f) = res_vtable.res_readonly else {
        return core::ptr::null();
    };
    // SAFETY: `raw` is non-null and `resource_name` is a valid nul-terminated `CStr`.
    unsafe { f(raw, resource_name.as_ptr()) }
}

/// Resolves a mutable resource byte pointer from `raw` via `res_vtable`.
///
/// # Safety
/// If `raw` is non-null, it must be a valid host [`RawChunkIter`] handle.
#[inline]
pub unsafe fn raw_iter_resource_mutable(
    raw: *mut RawChunkIter,
    res_vtable: &ResourceBridgeVTable,
    resource_name: &CStr,
) -> *mut u8 {
    if raw.is_null() {
        return core::ptr::null_mut();
    }
    let Some(f) = res_vtable.res_mutable else {
        return core::ptr::null_mut();
    };
    // SAFETY: `raw` is non-null and exclusively borrowed, and `resource_name` is a valid `CStr`.
    unsafe { f(raw, resource_name.as_ptr()) }
}

/// Validates alignment and non-nullness of `ptr` and converts it into `&'a R`.
///
/// # Safety
/// If `ptr` is non-null, it must point to an initialized `R` valid for shared reads over `'a`.
#[inline]
pub unsafe fn raw_deref_resource<'a, R: AbiPod>(ptr: *const u8) -> Option<&'a R> {
    if ptr.is_null() {
        return None;
    }
    if core::mem::size_of::<R>() == 0 {
        // SAFETY: ZST requires no backing bytes; lifetime is tied to caller's `'a`.
        return Some(unsafe { &*core::ptr::NonNull::<R>::dangling().as_ptr() });
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
        return None;
    }
    // SAFETY: `ptr` is non-null, aligned for `R: AbiPod` (every bit pattern valid), and backed by
    // host resource storage for `'a`.
    Some(unsafe { &*ptr.cast::<R>() })
}

/// Validates alignment and non-nullness of `ptr` and converts it into `&'a mut R`.
///
/// # Safety
/// If `ptr` is non-null, it must point to an initialized `R` exclusively borrowed over `'a`.
#[inline]
pub unsafe fn raw_deref_resource_mut<'a, R: AbiPod>(ptr: *mut u8) -> Option<&'a mut R> {
    if ptr.is_null() {
        return None;
    }
    if core::mem::size_of::<R>() == 0 {
        // SAFETY: ZST requires no backing bytes; exclusive borrow tied to caller's `'a`.
        return Some(unsafe { &mut *core::ptr::NonNull::<R>::dangling().as_ptr() });
    }
    if !(ptr as usize).is_multiple_of(core::mem::align_of::<R>()) {
        return None;
    }
    // SAFETY: `ptr` is non-null, aligned for `R: AbiPod`, and exclusively borrowed for `'a`.
    Some(unsafe { &mut *ptr.cast::<R>() })
}

/// Bounds-checked element pointer offset into a read-only column buffer.
///
/// # Safety
/// If `ptr` is non-null and `index < count`, `ptr` must belong to an allocation of at least
/// `(index + 1) * stride` bytes.
#[inline]
pub unsafe fn raw_slice_get_ptr(
    ptr: *const u8,
    stride: usize,
    count: usize,
    index: usize,
) -> Option<*const u8> {
    if index < count && !ptr.is_null() {
        let offset = index.checked_mul(stride)?;
        if offset > (isize::MAX as usize) {
            return None;
        }
        (ptr as usize).checked_add(offset)?;
        // SAFETY: Bounds checked (`index < count`), `offset <= isize::MAX` without address wrapping.
        unsafe { Some(ptr.add(offset)) }
    } else {
        None
    }
}

/// Bounds-checked element pointer offset into a mutable column buffer.
///
/// # Safety
/// If `ptr` is non-null and `index < count`, `ptr` must belong to an allocation of at least
/// `(index + 1) * stride` bytes.
#[inline]
pub unsafe fn raw_slice_get_ptr_mut(
    ptr: *mut u8,
    stride: usize,
    count: usize,
    index: usize,
) -> Option<*mut u8> {
    if index < count && !ptr.is_null() {
        let offset = index.checked_mul(stride)?;
        if offset > (isize::MAX as usize) {
            return None;
        }
        (ptr as usize).checked_add(offset)?;
        // SAFETY: Bounds checked (`index < count`), `offset <= isize::MAX` without address wrapping.
        unsafe { Some(ptr.add(offset)) }
    } else {
        None
    }
}

/// Validates `(ptr, stride, count)` and constructs `&'a [T]`.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized `T` elements
/// valid for shared reads over `'a`.
#[inline]
pub unsafe fn raw_slice_as_typed<'a, T: AbiPod>(
    ptr: *const u8,
    stride: usize,
    count: usize,
) -> Option<&'a [T]> {
    if stride != core::mem::size_of::<T>() {
        return None;
    }
    if count == 0 {
        return Some(&[]);
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    if count > (isize::MAX as usize) {
        return None;
    }
    let total_bytes = match count.checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
        _ => return None,
    };
    (ptr as usize).checked_add(total_bytes)?;
    // SAFETY: `ptr` is non-null, aligned for `T: AbiPod`, contiguous (`stride == size_of::<T>()`),
    // and `total_bytes <= isize::MAX` without address-space wrap-around.
    Some(unsafe { core::slice::from_raw_parts(ptr.cast::<T>(), count) })
}

/// Validates `(ptr, stride, count)` and constructs `&'a mut [T]`.
///
/// # Safety
/// If `ptr` is non-null and `count > 0`, `ptr` must point to `count` initialized `T` elements
/// exclusively borrowed over `'a`.
#[inline]
pub unsafe fn raw_slice_as_typed_mut<'a, T: AbiPod>(
    ptr: *mut u8,
    stride: usize,
    count: usize,
) -> Option<&'a mut [T]> {
    if stride != core::mem::size_of::<T>() {
        return None;
    }
    if count == 0 {
        return Some(&mut []);
    }
    if ptr.is_null() || !(ptr as usize).is_multiple_of(core::mem::align_of::<T>()) {
        return None;
    }
    if count > (isize::MAX as usize) {
        return None;
    }
    let total_bytes = match count.checked_mul(core::mem::size_of::<T>()) {
        Some(bytes) if bytes <= (isize::MAX as usize) => bytes,
        _ => return None,
    };
    (ptr as usize).checked_add(total_bytes)?;
    // SAFETY: `ptr` is non-null, aligned for `T: AbiPod`, contiguous (`stride == size_of::<T>()`),
    // `total_bytes <= isize::MAX` without wrap-around, and exclusively borrowed by caller for `'a`.
    Some(unsafe { core::slice::from_raw_parts_mut(ptr.cast::<T>(), count) })
}

/// Unforgeable validated read-only column slice wrapper.
#[derive(Clone, Copy, Debug)]
pub struct ValidatedSlice<'a> {
    ptr: *const u8,
    stride: usize,
    count: usize,
    _marker: PhantomData<&'a ()>,
}

impl<'a> ValidatedSlice<'a> {
    /// Creates an empty read-only slice with the given byte `stride`.
    #[inline]
    pub const fn empty(stride: usize) -> Self {
        Self {
            ptr: core::ptr::null(),
            stride,
            count: 0,
            _marker: PhantomData,
        }
    }

    /// Creates a read-only column slice from raw parts.
    ///
    /// # Safety
    /// - If `ptr` is non-null and `count > 0`, `ptr` must point to a valid, initialized allocation
    ///   of at least `count * stride` bytes valid for shared reads over `'a` (or, for `stride == 0`,
    ///   a non-null address valid for ZST slice construction).
    /// - No mutable alias to the underlying bytes may exist during `'a`.
    #[inline]
    pub unsafe fn new(ptr: *const u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: PhantomData,
        }
    }

    /// Returns the number of rows in this chunk slice.
    #[inline]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in this column.
    #[inline]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if `count == 0` or the underlying pointer is null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a bounds-checked element byte pointer at `index`.
    #[inline]
    pub fn get_ptr(&self, index: usize) -> Option<*const u8> {
        // SAFETY: `self.ptr`, `self.stride`, and `self.count` were validated on construction.
        unsafe { raw_slice_get_ptr(self.ptr, self.stride, self.count, index) }
    }

    /// Converts this column slice into a typed `&'a [T]` slice of [`AbiPod`] elements.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&self) -> Option<&'a [T]> {
        // SAFETY: `self.ptr`, `self.stride`, and `self.count` were validated for shared reads over `'a`.
        unsafe { raw_slice_as_typed::<T>(self.ptr, self.stride, self.count) }
    }
}

/// Unforgeable validated mutable column slice wrapper.
#[derive(Debug)]
pub struct ValidatedSliceMut<'a> {
    ptr: *mut u8,
    stride: usize,
    count: usize,
    _marker: PhantomData<&'a mut ()>,
}

impl<'a> ValidatedSliceMut<'a> {
    /// Creates an empty mutable slice with the given byte `stride`.
    #[inline]
    pub const fn empty(stride: usize) -> Self {
        Self {
            ptr: core::ptr::null_mut(),
            stride,
            count: 0,
            _marker: PhantomData,
        }
    }

    /// Creates a mutable column slice from raw parts.
    ///
    /// # Safety
    /// - If `ptr` is non-null and `count > 0`, `ptr` must point to a valid, initialized allocation
    ///   of at least `count * stride` bytes exclusively borrowed over `'a` (or, for `stride == 0`,
    ///   a non-null address valid for ZST mutable slice construction).
    /// - No other shared or mutable alias to the underlying bytes may exist during `'a`.
    #[inline]
    pub unsafe fn new(ptr: *mut u8, stride: usize, count: usize) -> Self {
        Self {
            ptr,
            stride,
            count,
            _marker: PhantomData,
        }
    }

    /// Returns the number of rows in this chunk slice.
    #[inline]
    pub const fn count(&self) -> usize {
        self.count
    }

    /// Returns the byte stride between consecutive elements in this column.
    #[inline]
    pub const fn stride(&self) -> usize {
        self.stride
    }

    /// Returns `true` if `count == 0` or the underlying pointer is null.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.count == 0 || self.ptr.is_null()
    }

    /// Returns a bounds-checked mutable element byte pointer at `index`.
    #[inline]
    pub fn get_ptr_mut(&mut self, index: usize) -> Option<*mut u8> {
        // SAFETY: `self.ptr`, `self.stride`, and `self.count` were validated on construction.
        unsafe { raw_slice_get_ptr_mut(self.ptr, self.stride, self.count, index) }
    }

    /// Reborrows this mutable column slice as `&mut [T]` for `T: AbiPod`.
    #[inline]
    pub fn as_typed_slice<T: AbiPod>(&mut self) -> Option<&mut [T]> {
        // SAFETY: `self.ptr` is exclusively borrowed via `&mut self`.
        unsafe { raw_slice_as_typed_mut::<T>(self.ptr, self.stride, self.count) }
    }

    /// Consumes this `ValidatedSliceMut<'a>` and converts it into `&'a mut [T]` for `T: AbiPod`.
    #[inline]
    pub fn into_typed_slice<T: AbiPod>(self) -> Option<&'a mut [T]> {
        // SAFETY: `self.ptr` is exclusively borrowed for `'a`.
        unsafe { raw_slice_as_typed_mut::<T>(self.ptr, self.stride, self.count) }
    }
}

/// Unforgeable validated chunk iterator wrapper tracking the active chunk length internally.
pub struct ValidatedIter<'a> {
    raw: *mut RawChunkIter,
    api: &'a HostApi,
    reset_fn: Option<IterResetFn>,
    res_vtable: ResourceBridgeVTable,
    allow_resource_access: bool,
    current_len: usize,
}

impl<'a> ValidatedIter<'a> {
    /// Creates a validated chunk iterator wrapper from a raw handle and host API table.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - Every iterator function pointer in `api` must be safe to invoke with `raw`.
    #[inline]
    pub unsafe fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        Self {
            raw,
            api,
            reset_fn: None,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: true,
            current_len: 0,
        }
    }

    /// Creates a validated chunk iterator wrapper with an optional [`IterResetFn`].
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - If `reset_fn` is `Some`, it must be safe to invoke with `raw`.
    #[inline]
    pub unsafe fn with_reset(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: true,
            current_len: 0,
        }
    }

    /// Creates a validated chunk iterator wrapper with [`IterResetFn`] and [`ResourceBridgeVTable`].
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - Every non-None function pointer in `reset_fn` and `res_vtable` must be safe to invoke
    ///   with `raw`.
    #[inline]
    pub unsafe fn with_bridges(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
        res_vtable: ResourceBridgeVTable,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable,
            allow_resource_access: true,
            current_len: 0,
        }
    }

    #[inline]
    pub(crate) unsafe fn for_multi_query(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
    ) -> Self {
        Self {
            raw,
            api,
            reset_fn,
            res_vtable: ResourceBridgeVTable::default(),
            allow_resource_access: false,
            current_len: 0,
        }
    }

    /// Number of entities in the currently active chunk (`0` before advancing, on reset, or when exhausted).
    #[inline]
    pub const fn current_len(&self) -> usize {
        self.current_len
    }

    /// Attaches a resolved [`IterResetFn`] to enable [`ValidatedIter::reset`].
    ///
    /// # Safety
    /// If `reset_fn` is `Some`, it must be a valid host `iter_reset` symbol compatible with `self`.
    #[inline]
    pub unsafe fn set_reset_fn(&mut self, reset_fn: Option<IterResetFn>) {
        self.reset_fn = reset_fn;
    }

    /// Attaches a resolved [`ResourceBridgeVTable`] to enable resource lookups.
    ///
    /// # Safety
    /// Every non-None function pointer in `res_vtable` must be a valid host resource bridge symbol
    /// compatible with `self`.
    #[inline]
    pub unsafe fn set_resource_vtable(&mut self, res_vtable: ResourceBridgeVTable) {
        self.res_vtable = res_vtable;
    }

    /// Rewinds the chunk iterator cursor back to the first matched chunk and zeroes `current_len`.
    #[inline]
    pub fn reset(&mut self) -> FfiResult {
        self.current_len = 0;
        // SAFETY: `self.raw` and `self.reset_fn` were validated and exclusively borrowed via `&mut self`.
        unsafe { raw_iter_reset(self.raw, self.reset_fn) }
    }

    /// Rewinds the chunk iterator cursor using an explicitly supplied [`IterResetFn`] and zeroes `current_len`.
    ///
    /// # Safety
    /// `reset_fn` must be a valid host `iter_reset` function pointer compatible with this iterator's
    /// [`RawChunkIter`] handle.
    #[inline]
    pub unsafe fn reset_with(&mut self, reset_fn: IterResetFn) -> FfiResult {
        self.current_len = 0;
        // SAFETY: `self.raw` is exclusively borrowed and caller guarantees `reset_fn` is valid for `self.raw`.
        unsafe { raw_iter_reset(self.raw, Some(reset_fn)) }
    }

    /// Advances to the next archetype table chunk, updates `current_len`, and returns the entity count.
    #[inline]
    pub fn next_chunk(&mut self) -> usize {
        // SAFETY: `self.raw` and `self.api` were validated on construction and exclusively borrowed via `&mut self`.
        let count = unsafe { raw_iter_next_chunk(self.raw, self.api) };
        self.current_len = count;
        count
    }

    /// Obtains a read-only pointer to the start of column `index` in the current chunk.
    #[inline]
    pub fn column_readonly(&self, index: usize) -> *const u8 {
        // SAFETY: `self.raw` and `self.api` were validated on construction and borrowed via `&self`.
        unsafe { raw_iter_column_readonly(self.raw, self.api, index) }
    }

    /// Obtains a mutable pointer to the start of column `index` in the current chunk.
    #[inline]
    pub fn column_mutable(&mut self, index: usize) -> *mut u8 {
        // SAFETY: `self.raw` and `self.api` were validated on construction and exclusively borrowed via `&mut self`.
        unsafe { raw_iter_column_mutable(self.raw, self.api, index) }
    }

    /// Marks `row` within column `index` as changed.
    #[inline]
    pub fn mark_changed(&mut self, index: usize, row: usize) -> FfiResult {
        // SAFETY: `self.raw` and `self.api` were validated on construction and exclusively borrowed via `&mut self`.
        unsafe { raw_iter_mark_changed(self.raw, self.api, index, row) }
    }

    /// Obtains a raw pointer to the contiguous array of [`FfiEntity`] values in the current chunk.
    #[inline]
    pub fn entities(&self) -> *const FfiEntity {
        // SAFETY: `self.raw` and `self.api` were validated on construction and borrowed via `&self`.
        unsafe { raw_iter_entities(self.raw, self.api) }
    }

    /// Returns the validated [`FfiEntity`] slice for the current chunk using `current_len`.
    #[inline]
    pub fn entities_slice(&self) -> &[FfiEntity] {
        let count = self.current_len;
        if self.raw.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: `self.entities()` yields the active chunk's entity column pointer for `self.current_len` rows.
        unsafe { validated_entities_slice(self.entities(), count) }
    }

    /// Unchecked entity slice construction using a caller-supplied `count`.
    ///
    /// # Safety
    /// Caller must ensure `count` does not exceed the number of rows in the currently active chunk.
    #[inline]
    pub unsafe fn entities_slice_unchecked(&self, count: usize) -> &[FfiEntity] {
        if self.raw.is_null() || count == 0 {
            return &[];
        }
        // SAFETY: Caller guarantees `count` is within the active chunk's row count.
        unsafe { validated_entities_slice(self.entities(), count) }
    }

    /// Returns the byte stride for elements in column `index`.
    #[inline]
    pub fn column_stride(&self, index: usize) -> usize {
        // SAFETY: `self.raw` and `self.api` were validated on construction and borrowed via `&self`.
        unsafe { raw_iter_column_stride(self.raw, self.api, index) }
    }

    /// Returns a bounds-checked read-only [`ValidatedSlice`] for column `index` using `current_len`.
    #[inline]
    pub fn readonly_slice(&self, index: usize) -> ValidatedSlice<'_> {
        let count = self.current_len;
        let stride = self.column_stride(index);
        if count == 0 {
            return ValidatedSlice::empty(stride);
        }
        let ptr = self.column_readonly(index);
        // SAFETY: `ptr` and `stride` come from the validated host chunk iterator for `self.current_len` rows,
        // tied to `&self`.
        unsafe { ValidatedSlice::new(ptr, stride, count) }
    }

    /// Unchecked read-only slice construction using a caller-supplied `count`.
    ///
    /// # Safety
    /// Caller must ensure `count` does not exceed the number of rows in the currently active chunk.
    #[inline]
    pub unsafe fn readonly_slice_unchecked(
        &self,
        index: usize,
        count: usize,
    ) -> ValidatedSlice<'_> {
        let ptr = self.column_readonly(index);
        let stride = self.column_stride(index);
        // SAFETY: Caller guarantees `count` does not exceed the active chunk's row count.
        unsafe { ValidatedSlice::new(ptr, stride, count) }
    }

    /// Returns a bounds-checked mutable [`ValidatedSliceMut`] for column `index` using `current_len`.
    #[inline]
    pub fn mutable_slice(&mut self, index: usize) -> ValidatedSliceMut<'_> {
        let count = self.current_len;
        let stride = self.column_stride(index);
        if count == 0 {
            return ValidatedSliceMut::empty(stride);
        }
        let ptr = self.column_mutable(index);
        // SAFETY: `ptr` and `stride` come from the validated host chunk iterator for `self.current_len` rows,
        // exclusively borrowed via `&mut self`.
        unsafe { ValidatedSliceMut::new(ptr, stride, count) }
    }

    /// Unchecked mutable slice construction using a caller-supplied `count`.
    ///
    /// # Safety
    /// Caller must ensure `count` does not exceed the number of rows in the currently active chunk.
    #[inline]
    pub unsafe fn mutable_slice_unchecked(
        &mut self,
        index: usize,
        count: usize,
    ) -> ValidatedSliceMut<'_> {
        let ptr = self.column_mutable(index);
        let stride = self.column_stride(index);
        // SAFETY: Caller guarantees `count` does not exceed the active chunk's row count.
        unsafe { ValidatedSliceMut::new(ptr, stride, count) }
    }

    /// Simultaneously borrows `entities`, `R` read-only columns, and `W` disjoint mutable columns
    /// from the active chunk using `current_len`.
    #[allow(clippy::type_complexity)]
    pub fn split_slices<const R: usize, const W: usize>(
        &mut self,
        read_cols: [usize; R],
        write_cols: [usize; W],
    ) -> Option<(
        &[FfiEntity],
        [ValidatedSlice<'_>; R],
        [ValidatedSliceMut<'_>; W],
    )> {
        for i in 0..W {
            for j in (i + 1)..W {
                if write_cols[i] == write_cols[j] {
                    return None;
                }
            }
            for &r_col in &read_cols {
                if r_col == write_cols[i] {
                    return None;
                }
            }
        }

        let count = self.current_len;
        if self.raw.is_null() && count > 0 {
            return None;
        }

        let ent_ptr = self.entities();
        // SAFETY: `ent_ptr` is the active chunk's entity column pointer for `self.current_len` rows.
        let ent_slice = unsafe { validated_entities_slice(ent_ptr, count) };
        let ent_addr = ent_slice.as_ptr() as usize;
        let ent_bytes = core::mem::size_of_val(ent_slice);

        let mut ro_ptrs = [core::ptr::null::<u8>(); R];
        let mut ro_strides = [0usize; R];
        let mut ro_bytes = [0usize; R];

        for i in 0..R {
            let ptr = self.column_readonly(read_cols[i]);
            let stride = self.column_stride(read_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            ro_ptrs[i] = ptr;
            ro_strides[i] = stride;
            ro_bytes[i] = bytes;
        }

        let mut rw_ptrs = [core::ptr::null_mut::<u8>(); W];
        let mut rw_strides = [0usize; W];
        let mut rw_bytes = [0usize; W];

        for i in 0..W {
            let ptr = self.column_mutable(write_cols[i]);
            let stride = self.column_stride(write_cols[i]);
            if count > 0 && ptr.is_null() {
                return None;
            }
            let bytes = count.checked_mul(stride)?;
            if bytes > (isize::MAX as usize) {
                return None;
            }
            if count > 0 && (ptr as usize).checked_add(bytes).is_none() {
                return None;
            }
            rw_ptrs[i] = ptr;
            rw_strides[i] = stride;
            rw_bytes[i] = bytes;
        }

        if count > 0 {
            for i in 0..W {
                let addr_w = rw_ptrs[i] as usize;
                let len_w = rw_bytes[i];

                if byte_ranges_overlap(addr_w, len_w, ent_addr, ent_bytes) {
                    return None;
                }
                for j in (i + 1)..W {
                    if byte_ranges_overlap(addr_w, len_w, rw_ptrs[j] as usize, rw_bytes[j]) {
                        return None;
                    }
                }
                for r in 0..R {
                    if byte_ranges_overlap(addr_w, len_w, ro_ptrs[r] as usize, ro_bytes[r]) {
                        return None;
                    }
                }
            }
        }

        let ro_slices = core::array::from_fn(|i| {
            // SAFETY: Each read-only column pointer is non-overlapping with every mutable column
            // and valid for `count` (`self.current_len`) rows during `&mut self`.
            unsafe { ValidatedSlice::new(ro_ptrs[i], ro_strides[i], count) }
        });
        let rw_slices = core::array::from_fn(|i| {
            // SAFETY: Each mutable column pointer is pairwise disjoint from all other columns and
            // entities, and valid for `count` (`self.current_len`) rows during `&mut self`.
            unsafe { ValidatedSliceMut::new(rw_ptrs[i], rw_strides[i], count) }
        });

        Some((ent_slice, ro_slices, rw_slices))
    }

    /// Borrows `N` disjoint mutable columns simultaneously from the active chunk using `current_len`.
    #[inline]
    pub fn columns_mut<const N: usize>(
        &mut self,
        indices: [usize; N],
    ) -> Option<[ValidatedSliceMut<'_>; N]> {
        let (_, _, rw) = self.split_slices([], indices)?;
        Some(rw)
    }

    /// Simultaneously borrows one typed read-only column (`&[R]`) and one typed mutable column (`&mut [W]`).
    #[inline]
    pub fn read_write<R: AbiPod, W: AbiPod>(
        &mut self,
        read_col: usize,
        write_col: usize,
    ) -> Option<(&[R], &mut [W])> {
        let (_, [ro], [rw]) = self.split_slices([read_col], [write_col])?;
        let r_typed = ro.as_typed_slice::<R>()?;
        let w_typed = rw.into_typed_slice::<W>()?;
        Some((r_typed, w_typed))
    }

    /// Simultaneously borrows two disjoint typed mutable columns (`&mut [W1]`, `&mut [W2]`).
    #[inline]
    pub fn write_2<W1: AbiPod, W2: AbiPod>(
        &mut self,
        col_a: usize,
        col_b: usize,
    ) -> Option<(&mut [W1], &mut [W2])> {
        let [rw_a, rw_b] = self.columns_mut([col_a, col_b])?;
        let a_typed = rw_a.into_typed_slice::<W1>()?;
        let b_typed = rw_b.into_typed_slice::<W2>()?;
        Some((a_typed, b_typed))
    }

    /// Obtains an optional read-only column pointer for the current chunk.
    #[inline]
    pub fn optional_column_ptr(&self, index: usize) -> Option<*const u8> {
        let ptr = self.column_readonly(index);
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// Obtains an optional mutable column pointer for the current chunk.
    #[inline]
    pub fn optional_column_mut_ptr(&mut self, index: usize) -> Option<*mut u8> {
        let ptr = self.column_mutable(index);
        if ptr.is_null() { None } else { Some(ptr) }
    }

    /// Returns `Some(ValidatedSlice)` when an `OPTIONAL_READ` column is present on the current chunk.
    #[inline]
    pub fn optional_readonly_slice(&self, index: usize) -> Option<ValidatedSlice<'_>> {
        let ptr = self.optional_column_ptr(index)?;
        let stride = self.column_stride(index);
        // SAFETY: `ptr` is non-null and valid for `self.current_len` rows over `&self`.
        Some(unsafe { ValidatedSlice::new(ptr, stride, self.current_len) })
    }

    /// Returns `Some(ValidatedSliceMut)` when an `OPTIONAL_WRITE` column is present on the current chunk.
    #[inline]
    pub fn optional_mutable_slice(&mut self, index: usize) -> Option<ValidatedSliceMut<'_>> {
        let ptr = self.optional_column_mut_ptr(index)?;
        let stride = self.column_stride(index);
        // SAFETY: `ptr` is non-null and exclusively borrowed for `self.current_len` rows over `&mut self`.
        Some(unsafe { ValidatedSliceMut::new(ptr, stride, self.current_len) })
    }

    /// Simultaneously borrows an optional read-only column (`Option<&[OptR]>`) and a required
    /// disjoint mutable column (`&mut [W]`) using `current_len`.
    pub fn optional_read_write<OptR: AbiPod, W: AbiPod>(
        &mut self,
        opt_read_col: usize,
        write_col: usize,
    ) -> Option<(Option<&[OptR]>, &mut [W])> {
        if opt_read_col == write_col {
            return None;
        }
        let count = self.current_len;
        if self.raw.is_null() && count > 0 {
            return None;
        }

        let opt_ptr = self.column_readonly(opt_read_col);
        let opt_stride = self.column_stride(opt_read_col);

        let w_ptr = self.column_mutable(write_col);
        let w_stride = self.column_stride(write_col);
        if count > 0 && w_ptr.is_null() {
            return None;
        }
        let w_bytes = count.checked_mul(w_stride)?;
        if w_bytes > (isize::MAX as usize)
            || (count > 0 && (w_ptr as usize).checked_add(w_bytes).is_none())
        {
            return None;
        }

        let opt_slice = if opt_ptr.is_null() {
            None
        } else {
            let opt_bytes = count.checked_mul(opt_stride)?;
            if opt_bytes > (isize::MAX as usize)
                || (count > 0 && (opt_ptr as usize).checked_add(opt_bytes).is_none())
            {
                return None;
            }
            if count > 0
                && byte_ranges_overlap(w_ptr as usize, w_bytes, opt_ptr as usize, opt_bytes)
            {
                return None;
            }
            // SAFETY: `opt_ptr` is disjoint from `w_ptr` and valid for `count` rows over `&mut self`.
            let raw_ro = unsafe { ValidatedSlice::new(opt_ptr, opt_stride, count) };
            Some(raw_ro.as_typed_slice::<OptR>()?)
        };

        // SAFETY: `w_ptr` is disjoint from `opt_ptr` and valid for `count` rows over `&mut self`.
        let raw_rw = unsafe { ValidatedSliceMut::new(w_ptr, w_stride, count) };
        let w_typed = raw_rw.into_typed_slice::<W>()?;
        Some((opt_slice, w_typed))
    }

    /// Obtains a read-only byte pointer to an exposed Bevy `Resource` (`Res<R>`).
    #[inline]
    pub fn resource_ptr(&self, resource_name: &CStr) -> *const u8 {
        if !self.allow_resource_access {
            return core::ptr::null();
        }
        // SAFETY: `self.raw` and `self.res_vtable` were validated on construction and borrowed via `&self`.
        unsafe { raw_iter_resource_readonly(self.raw, &self.res_vtable, resource_name) }
    }

    /// Obtains a mutable byte pointer to an exposed Bevy `Resource` (`ResMut<R>`).
    #[inline]
    pub fn resource_mut_ptr(&mut self, resource_name: &CStr) -> *mut u8 {
        if !self.allow_resource_access {
            return core::ptr::null_mut();
        }
        // SAFETY: `self.raw` and `self.res_vtable` were validated on construction and exclusively borrowed via `&mut self`.
        unsafe { raw_iter_resource_mutable(self.raw, &self.res_vtable, resource_name) }
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`Res<R>`) for reading.
    #[inline]
    pub fn resource<R: AbiPod>(&self, resource_name: &CStr) -> Option<&R> {
        // SAFETY: `resource_ptr` returns null or a valid host resource pointer for `&self`.
        unsafe { raw_deref_resource::<R>(self.resource_ptr(resource_name)) }
    }

    /// Safely borrows a typed [`AbiPod`] Bevy `Resource` (`ResMut<R>`) mutably.
    #[inline]
    pub fn resource_mut<R: AbiPod>(&mut self, resource_name: &CStr) -> Option<&mut R> {
        // SAFETY: `resource_mut_ptr` returns null or an exclusively borrowed host resource pointer for `&mut self`.
        unsafe { raw_deref_resource_mut::<R>(self.resource_mut_ptr(resource_name)) }
    }

    /// Queries a read-only resource byte pointer through an explicit [`ResourceBridgeVTable`]
    /// (used by `MultiQueryContext`, which enforces top-level borrow exclusivity across query groups).
    #[inline]
    pub fn multi_query_resource_readonly(
        &self,
        res_vtable: &ResourceBridgeVTable,
        resource_name: &CStr,
    ) -> *const u8 {
        // SAFETY: `self.raw` is a validated host iterator handle borrowed via `&self`, and
        // `res_vtable` was resolved from the same validated host init session.
        unsafe { raw_iter_resource_readonly(self.raw, res_vtable, resource_name) }
    }

    /// Queries a mutable resource byte pointer through an explicit [`ResourceBridgeVTable`]
    /// (used by `MultiQueryContext`, which enforces top-level `&mut` borrow exclusivity).
    #[inline]
    pub fn multi_query_resource_mutable(
        &mut self,
        res_vtable: &ResourceBridgeVTable,
        resource_name: &CStr,
    ) -> *mut u8 {
        // SAFETY: `self.raw` is a validated host iterator handle exclusively borrowed via `&mut self`,
        // and `res_vtable` was resolved from the same validated host init session.
        unsafe { raw_iter_resource_mutable(self.raw, res_vtable, resource_name) }
    }

    /// Queries a typed read-only [`AbiPod`] resource through an explicit [`ResourceBridgeVTable`].
    #[inline]
    pub fn multi_query_resource<R: AbiPod>(
        &self,
        res_vtable: &ResourceBridgeVTable,
        resource_name: &CStr,
    ) -> Option<&R> {
        let ptr = self.multi_query_resource_readonly(res_vtable, resource_name);
        // SAFETY: `ptr` is null or a valid host resource pointer borrowed for `&self`.
        unsafe { raw_deref_resource::<R>(ptr) }
    }

    /// Queries a typed mutable [`AbiPod`] resource through an explicit [`ResourceBridgeVTable`].
    #[inline]
    pub fn multi_query_resource_mut<R: AbiPod>(
        &mut self,
        res_vtable: &ResourceBridgeVTable,
        resource_name: &CStr,
    ) -> Option<&mut R> {
        let ptr = self.multi_query_resource_mutable(res_vtable, resource_name);
        // SAFETY: `ptr` is null or an exclusively borrowed host resource pointer for `&mut self`.
        unsafe { raw_deref_resource_mut::<R>(ptr) }
    }
}

/// Bridge trait implemented by the safe SDK's `ChunkSlice` to expose raw `unsafe` construction.
pub trait ValidatedSliceAdapter<'a>: Sized {
    /// Wraps an unforgeable [`ValidatedSlice`] into `Self`.
    fn from_validated_slice(inner: ValidatedSlice<'a>) -> Self;
}

/// Raw `unsafe` constructor extension for `ChunkSlice`.
pub trait ChunkSliceRawExt<'a>: ValidatedSliceAdapter<'a> {
    /// Creates a read-only chunk slice view from raw parts.
    ///
    /// # Safety
    /// - If `ptr` is non-null and `count > 0`, `ptr` must point to a valid, initialized allocation
    ///   of at least `count * stride` bytes valid for shared reads over `'a` (or, for `stride == 0`,
    ///   a non-null address valid for ZST slice construction).
    /// - No mutable alias to the underlying bytes may exist during `'a`.
    #[inline]
    unsafe fn new(ptr: *const u8, stride: usize, count: usize) -> Self {
        // SAFETY: Forwarding caller's raw slice invariants to `ValidatedSlice::new`.
        Self::from_validated_slice(unsafe { ValidatedSlice::new(ptr, stride, count) })
    }
}

impl<'a, T: ValidatedSliceAdapter<'a>> ChunkSliceRawExt<'a> for T {}

/// Bridge trait implemented by the safe SDK's `ChunkSliceMut` to expose raw `unsafe` construction.
pub trait ValidatedSliceMutAdapter<'a>: Sized {
    /// Wraps an unforgeable [`ValidatedSliceMut`] into `Self`.
    fn from_validated_slice_mut(inner: ValidatedSliceMut<'a>) -> Self;
}

/// Raw `unsafe` constructor extension for `ChunkSliceMut`.
pub trait ChunkSliceMutRawExt<'a>: ValidatedSliceMutAdapter<'a> {
    /// Creates a mutable chunk slice view from raw parts.
    ///
    /// # Safety
    /// - If `ptr` is non-null and `count > 0`, `ptr` must point to a valid, initialized allocation
    ///   of at least `count * stride` bytes exclusively borrowed over `'a` (or, for `stride == 0`,
    ///   a non-null address valid for ZST mutable slice construction).
    /// - No other shared or mutable alias to the underlying bytes may exist during `'a`.
    #[inline]
    unsafe fn new(ptr: *mut u8, stride: usize, count: usize) -> Self {
        // SAFETY: Forwarding caller's raw mutable slice invariants to `ValidatedSliceMut::new`.
        Self::from_validated_slice_mut(unsafe { ValidatedSliceMut::new(ptr, stride, count) })
    }
}

impl<'a, T: ValidatedSliceMutAdapter<'a>> ChunkSliceMutRawExt<'a> for T {}

/// Bridge trait implemented by the safe SDK's `GuestChunkIter` to expose raw `unsafe` helpers.
pub trait ValidatedIterAdapter<'a>: Sized {
    /// Wraps an unforgeable [`ValidatedIter`] into `Self`.
    fn from_validated_iter(inner: ValidatedIter<'a>) -> Self;
    /// Borrows the underlying [`ValidatedIter`] mutably.
    fn as_validated_iter_mut(&mut self) -> &mut ValidatedIter<'a>;
}

/// Raw `unsafe` constructors and raw function-pointer setters for `GuestChunkIter`.
pub trait GuestChunkIterRawExt<'a>: ValidatedIterAdapter<'a> {
    /// Creates a chunk iterator wrapper from a raw handle and host API table.
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - Every iterator function pointer in `api` must be safe to invoke with `raw`.
    #[inline]
    unsafe fn new(raw: *mut RawChunkIter, api: &'a HostApi) -> Self {
        // SAFETY: Forwarding caller's raw iterator invariants.
        Self::from_validated_iter(unsafe { ValidatedIter::new(raw, api) })
    }

    /// Creates a chunk iterator wrapper with a pre-resolved [`IterResetFn`].
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - If `reset_fn` is `Some`, it must be safe to invoke with `raw`.
    #[inline]
    unsafe fn with_reset(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
    ) -> Self {
        // SAFETY: Forwarding caller's raw iterator and `reset_fn` invariants.
        Self::from_validated_iter(unsafe { ValidatedIter::with_reset(raw, api, reset_fn) })
    }

    /// Creates a chunk iterator wrapper with both [`IterResetFn`] and [`ResourceBridgeVTable`].
    ///
    /// # Safety
    /// - `raw` must be either null or a valid, exclusively borrowed [`RawChunkIter`] handle
    ///   produced by the host matching `api` for `'a`.
    /// - Every non-None function pointer in `reset_fn` and `res_vtable` must be safe to invoke
    ///   with `raw`.
    #[inline]
    unsafe fn with_bridges(
        raw: *mut RawChunkIter,
        api: &'a HostApi,
        reset_fn: Option<IterResetFn>,
        res_vtable: ResourceBridgeVTable,
    ) -> Self {
        // SAFETY: Forwarding caller's raw iterator and bridge VTable invariants.
        Self::from_validated_iter(unsafe {
            ValidatedIter::with_bridges(raw, api, reset_fn, res_vtable)
        })
    }

    /// Attaches a resolved [`IterResetFn`] to enable `reset()`.
    ///
    /// # Safety
    /// If `reset_fn` is `Some`, it must be a valid host `iter_reset` symbol compatible with `self`.
    #[inline]
    unsafe fn set_reset_fn(&mut self, reset_fn: Option<IterResetFn>) {
        // SAFETY: Forwarding caller's symbol invariant.
        unsafe { self.as_validated_iter_mut().set_reset_fn(reset_fn) };
    }

    /// Attaches a resolved [`ResourceBridgeVTable`] to enable resource lookups.
    ///
    /// # Safety
    /// Every non-None function pointer in `res_vtable` must be a valid host resource bridge symbol
    /// compatible with `self`.
    #[inline]
    unsafe fn set_resource_vtable(&mut self, res_vtable: ResourceBridgeVTable) {
        // SAFETY: Forwarding caller's bridge VTable invariant.
        unsafe { self.as_validated_iter_mut().set_resource_vtable(res_vtable) };
    }

    /// Rewinds the chunk iterator cursor using an explicitly supplied [`IterResetFn`].
    ///
    /// # Safety
    /// `reset_fn` must be a valid host `iter_reset` function pointer compatible with `self`.
    #[inline]
    unsafe fn reset_with(&mut self, reset_fn: IterResetFn) -> FfiResult {
        // SAFETY: Forwarding caller's `reset_fn` invariant.
        unsafe { self.as_validated_iter_mut().reset_with(reset_fn) }
    }
}

impl<'a, T: ValidatedIterAdapter<'a>> GuestChunkIterRawExt<'a> for T {}
