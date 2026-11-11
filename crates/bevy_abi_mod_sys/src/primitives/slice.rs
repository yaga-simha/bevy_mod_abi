//! Non-owning `#[repr(C)]` slice and UTF-8 string view representations.

use crate::primitives::FfiResult;

/// ABI-safe non-owning slice representation (`#[repr(C)]` replacement for `&[T]`).
///
/// # Borrow Lifetime Contract
/// When returned by a generated component VTable getter, the `FfiSlice` borrows directly from
/// host memory (table columns or heap allocations) and remains valid only until the next mutation
/// of that field, structural ECS change, or the end of the current system callback.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{FfiSlice, StrView};
///
/// let values = [10u32, 20, 30];
/// let slice = FfiSlice::from_slice(&values);
/// assert_eq!(slice.len, 3);
/// // SAFETY: `slice` borrows `values` on the local stack.
/// unsafe {
///     assert_eq!(slice.as_slice(), &[10, 20, 30]);
/// }
///
/// let text = "bevy_abi_mod";
/// let view = StrView::from_str(text);
/// // SAFETY: `view` borrows `text` with valid UTF-8 bytes.
/// unsafe {
///     assert_eq!(view.try_as_str().unwrap(), "bevy_abi_mod");
/// }
/// ```
#[repr(C)]
#[derive(Debug)]
pub struct FfiSlice<T> {
    /// Pointer to the start of the contiguous array of elements (may be null iff `len == 0`).
    pub ptr: *const T,
    /// Number of elements in the slice.
    pub len: usize,
}

impl<T> Copy for FfiSlice<T> {}

impl<T> Clone for FfiSlice<T> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Default for FfiSlice<T> {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl<T> FfiSlice<T> {
    /// Constructs an empty `FfiSlice` with a non-null dangling pointer and zero length.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ptr: core::ptr::NonNull::<T>::dangling().as_ptr(),
            len: 0,
        }
    }

    /// Constructs an `FfiSlice` from a Rust slice reference.
    #[inline]
    pub const fn from_slice(slice: &[T]) -> Self {
        Self {
            ptr: slice.as_ptr(),
            len: slice.len(),
        }
    }

    /// Returns `true` if `self.len == 0`.
    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Validates pointer non-nullness, alignment, and byte-size overflow for `FfiSlice<T>`.
    ///
    /// Empty C slices `(NULL, 0)` are accepted (`FfiResult::OK`). Non-empty null slices return
    /// [`FfiResult::NULL_POINTER`]; unaligned non-null pointers or wrapping byte lengths return
    /// [`FfiResult::INVALID_ARGUMENT`].
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if self.len > 0 && self.ptr.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.ptr.is_null() && !self.ptr.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        let elem_size = core::mem::size_of::<T>();
        match self.len.checked_mul(elem_size) {
            Some(total_bytes) if total_bytes <= isize::MAX as usize => FfiResult::OK,
            _ => FfiResult::INVALID_ARGUMENT,
        }
    }

    /// Converts this `FfiSlice<T>` into a Rust slice reference, substituting `NonNull::dangling()`
    /// when `self.len == 0` so empty C slices `(NULL, 0)` never pass a null pointer to
    /// `core::slice::from_raw_parts`.
    ///
    /// # Safety
    /// - If `self.len > 0`, `self.ptr` must be non-null, properly aligned for `T`, and point to
    ///   `self.len` contiguous initialized values of type `T` totaling at most `isize::MAX` bytes.
    /// - The underlying memory must not be mutated for the duration of lifetime `'a`.
    #[inline]
    pub unsafe fn as_slice<'a>(&self) -> &'a [T] {
        if self.len == 0 {
            let dangling = core::ptr::NonNull::<T>::dangling().as_ptr();
            // SAFETY: `dangling` is non-null and aligned for `T`, and `len == 0`.
            unsafe { core::slice::from_raw_parts(dangling, 0) }
        } else {
            // SAFETY: Caller upholds non-null, aligned, initialized slice invariants for `'a`.
            unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    /// Validates nullness, alignment, and byte-length bounds before converting into `&'a [T]`.
    ///
    /// # Safety
    /// - If `self.validate().is_ok()` and `self.len > 0`, `self.ptr` must point to `self.len`
    ///   contiguous initialized values of type `T` that remain immutable for lifetime `'a`.
    #[inline]
    pub unsafe fn try_as_slice<'a>(&self) -> Result<&'a [T], FfiResult> {
        let status = self.validate();
        if status.is_err() {
            return Err(status);
        }
        // SAFETY: `validate()` checked nullness, alignment, and `isize::MAX` bounds; caller
        // guarantees initialized elements for `'a`.
        Ok(unsafe { self.as_slice() })
    }
}

impl FfiSlice<u8> {
    /// Constructs an `FfiSlice<u8>` from a `&str`.
    #[inline]
    pub const fn from_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// Views this byte slice as a UTF-8 `&str` without upfront pointer validation.
    ///
    /// # Safety
    /// - If `self.len > 0`, `self.ptr` must be non-null and point to `self.len` initialized bytes
    ///   totaling at most `isize::MAX` bytes that remain immutable for lifetime `'a`.
    #[inline]
    pub unsafe fn as_str<'a>(&self) -> Result<&'a str, core::str::Utf8Error> {
        // SAFETY: Caller upholds byte slice validity for lifetime `'a`.
        let bytes = unsafe { self.as_slice() };
        core::str::from_utf8(bytes)
    }

    /// Validates pointer bounds and UTF-8 encoding, returning [`FfiResult::NULL_POINTER`],
    /// [`FfiResult::INVALID_ARGUMENT`], or [`FfiResult::INVALID_UTF8`] on failure.
    ///
    /// # Safety
    /// - If `self.len > 0` and `!self.ptr.is_null()`, `self.ptr` must point to `self.len`
    ///   initialized bytes that remain immutable for lifetime `'a`.
    #[inline]
    pub unsafe fn try_as_str<'a>(&self) -> Result<&'a str, FfiResult> {
        // SAFETY: Forwarding under caller's initialized-bytes contract.
        let bytes = unsafe { self.try_as_slice() }?;
        core::str::from_utf8(bytes).map_err(|_| FfiResult::INVALID_UTF8)
    }
}

/// Non-owning UTF-8 string view across the C ABI boundary (`FfiSlice<u8>` alias; C `BamStrView`).
pub type StrView = FfiSlice<u8>;

const _: () = {
    #[cfg(target_pointer_width = "64")]
    {
        assert!(core::mem::size_of::<StrView>() == 16);
        assert!(core::mem::align_of::<StrView>() == 8);
        assert!(core::mem::offset_of!(StrView, ptr) == 0);
        assert!(core::mem::offset_of!(StrView, len) == 8);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(core::mem::size_of::<StrView>() == 8);
        assert!(core::mem::align_of::<StrView>() == 4);
        assert!(core::mem::offset_of!(StrView, ptr) == 0);
        assert!(core::mem::offset_of!(StrView, len) == 4);
    }
};
