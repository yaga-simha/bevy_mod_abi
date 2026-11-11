//! Extensible `struct_size`-prefixed C-ABI descriptor reading helpers.

use core::ffi::c_void;

use crate::primitives::FfiResult;

/// Marker trait for `#[repr(C)]` ABI descriptor, manifest, or API table structs prefixed with
/// `struct_size: u32` at byte offset `0` whose fields are all valid when zero-filled.
///
/// Because every foreign callback across ABI `0.2` uses `Option<unsafe extern "C" fn(...)>`,
/// Rust's null-pointer niche optimization guarantees that all-bits-zero is a valid `None`
/// representation on every platform without raw pointer byte-probing.
///
/// # Safety
/// Implementing `ExtensibleStruct` asserts that:
/// 1. `Self` is `#[repr(C)]` and begins with `struct_size: u32` at byte offset `0`.
/// 2. `core::mem::size_of::<u32>() <= Self::MIN_V0_2_SIZE <= core::mem::size_of::<Self>()`.
/// 3. Every field in `Self` is valid when all-bits-zero (for example primitive integers,
///    `#[repr(transparent)]` integer newtypes, `[u8; 16]`, raw pointers `*const T` / `*mut T`,
///    or `Option<unsafe extern "C" fn(...)>` — never bare `fn` pointers, `bool`, `NonNull<T>`,
///    or Rust references).
pub unsafe trait ExtensibleStruct: Copy + 'static {
    /// Frozen minimum valid `struct_size` in bytes for ABI `0.2`, computed via
    /// `core::mem::offset_of!(Self, <last_v0_2_field>) + core::mem::size_of::<LastFieldTy>()`.
    const MIN_V0_2_SIZE: usize;
}

/// Reads a variable-length `#[repr(C)]` descriptor or API table across the FFI boundary:
/// - Returns `Err(FfiResult::NULL_POINTER)` if `ptr` is null.
/// - Returns `Err(FfiResult::INVALID_VERSION)` if `struct_size < T::MIN_V0_2_SIZE`.
/// - Copies `min(struct_size, size_of::<T>())` bytes into a zero-initialized `MaybeUninit<T>`,
///   zero-filling any newer trailing fields when `struct_size < size_of::<T>()` and ignoring
///   unknown future trailing bytes when `struct_size > size_of::<T>()`.
///
/// # Safety
/// If `ptr` is non-null and `struct_size >= T::MIN_V0_2_SIZE`, `ptr` must be readable for at
/// least `min(struct_size, core::mem::size_of::<T>())` bytes containing valid field representations
/// for `T`'s prefix.
#[inline]
pub unsafe fn read_extensible_struct<T: ExtensibleStruct>(
    ptr: *const u8,
    struct_size: usize,
) -> Result<T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if struct_size < T::MIN_V0_2_SIZE {
        return Err(FfiResult::INVALID_VERSION);
    }
    let mut slot = core::mem::MaybeUninit::<T>::zeroed();
    let copy_len = if struct_size < core::mem::size_of::<T>() {
        struct_size
    } else {
        core::mem::size_of::<T>()
    };
    // SAFETY: `ptr` is non-null and caller guarantees `copy_len` readable bytes; `slot` has
    // `size_of::<T>() >= copy_len` writable bytes on the local stack and cannot overlap `ptr`.
    unsafe {
        core::ptr::copy_nonoverlapping(ptr, slot.as_mut_ptr().cast::<u8>(), copy_len);
    }
    // SAFETY: Prefix `0..copy_len` holds valid caller bytes and trailing `copy_len..size_of::<T>()`
    // is zero-initialized, which is valid for every field of `T: ExtensibleStruct`.
    Ok(unsafe { slot.assume_init() })
}

/// Reads an extensible `#[repr(C)]` struct starting with `struct_size: u32` at byte offset `0`.
///
/// # Safety
/// If `ptr` is non-null, it must be readable for at least 4 bytes to inspect `struct_size`, and
/// if `struct_size >= T::MIN_V0_2_SIZE`, readable for `min(struct_size, size_of::<T>())` bytes.
#[inline]
pub unsafe fn read_descriptor<T: ExtensibleStruct>(ptr: *const c_void) -> Result<T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    // SAFETY: Caller guarantees at least 4 readable bytes at `ptr` for the leading `struct_size: u32`.
    let declared_size = unsafe { core::ptr::read_unaligned(ptr.cast::<u32>()) } as usize;
    // SAFETY: Forwarding `ptr` and its declared `struct_size` under caller's readability contract.
    unsafe { read_extensible_struct::<T>(ptr.cast::<u8>(), declared_size) }
}
