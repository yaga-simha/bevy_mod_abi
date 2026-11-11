//! Non-owning `#[repr(C)]` slice and UTF-8 string view representations.

/// ABI-safe non-owning slice representation (`#[repr(C)]` replacement for `&[T]`).
///
/// # Borrow Lifetime Contract
/// When returned by a generated component getter (e.g., `<Struct>_get_<field>`),
/// the returned `FfiSlice` borrows directly from host memory (table columns or heap allocations).
/// It remains valid only until the next mutation of that field, structural ECS change,
/// or the end of the current chunk callback.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{FfiSlice, StrView};
///
/// let values = [10u32, 20, 30];
/// let slice = FfiSlice::from_slice(&values);
/// assert_eq!(slice.len, 3);
/// assert!(!slice.is_empty());
/// // SAFETY: `slice` borrows `values` on the local stack.
/// unsafe {
///     assert_eq!(slice.as_slice(), &[10, 20, 30]);
/// }
///
/// let text = "bevy_abi_mod";
/// let view = StrView::from_str(text);
/// // SAFETY: `view` borrows `text` with valid UTF-8 bytes.
/// unsafe {
///     assert_eq!(view.as_str().unwrap(), "bevy_abi_mod");
/// }
/// ```
#[repr(C)]
#[derive(Debug)]
pub struct FfiSlice<T> {
    /// Pointer to the start of the contiguous array of elements.
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

impl<T> FfiSlice<T> {
    /// Construct an empty `FfiSlice` with a non-null dangling pointer and zero length.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ptr: core::ptr::NonNull::<T>::dangling().as_ptr(),
            len: 0,
        }
    }

    /// Construct an `FfiSlice` from a Rust slice reference.
    #[inline]
    pub const fn from_slice(slice: &[T]) -> Self {
        Self {
            ptr: slice.as_ptr(),
            len: slice.len(),
        }
    }

    /// Returns `true` if the slice has length 0 or a null pointer.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0 || self.ptr.is_null()
    }

    /// Convert this `FfiSlice<T>` into a Rust slice reference.
    ///
    /// # Safety
    /// - If `self.len > 0`, `self.ptr` must be non-null, properly aligned for `T`,
    ///   and point to `self.len` contiguous initialized values of type `T`.
    /// - The underlying memory must not be mutated for the duration of lifetime `'a`.
    #[inline]
    pub unsafe fn as_slice<'a>(&self) -> &'a [T] {
        if self.len == 0 || self.ptr.is_null() {
            &[]
        } else {
            // SAFETY: Caller guarantees `self.ptr` is valid, aligned, and initialized
            // for `self.len` elements of `T` for the lifetime `'a`.
            unsafe { core::slice::from_raw_parts(self.ptr, self.len) }
        }
    }
}

impl FfiSlice<u8> {
    /// Construct an `FfiSlice<u8>` from a string slice.
    #[inline]
    pub const fn from_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// Attempt to view this byte slice as a UTF-8 `&str`.
    ///
    /// # Safety
    /// - If `self.len > 0`, `self.ptr` must point to `self.len` valid initialized bytes
    ///   that remain immutable for lifetime `'a`.
    #[inline]
    pub unsafe fn as_str<'a>(&self) -> Result<&'a str, core::str::Utf8Error> {
        // SAFETY: Caller upholds byte slice validity for lifetime `'a`.
        let bytes = unsafe { self.as_slice() };
        core::str::from_utf8(bytes)
    }
}

/// Non-owning UTF-8 string view across the C ABI boundary (`FfiSlice<u8>` alias).
pub type StrView = FfiSlice<u8>;
