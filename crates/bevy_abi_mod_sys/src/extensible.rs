//! Extensible `struct_size`-prefixed C-ABI descriptor reading helpers.

use core::ffi::c_void;

use crate::primitives::FfiResult;

/// Marker trait for `#[repr(C)]` ABI descriptor or VTable structs prefixed with `struct_size: u32`
/// at byte offset `0` whose trailing fields beyond [`Self::MIN_V1_SIZE`] are valid when zero-filled.
///
/// # Safety
/// Implementing `ExtensibleStruct` asserts that:
/// 1. `Self` is `#[repr(C)]` and begins with `struct_size: u32` at byte offset `0`.
/// 2. `core::mem::size_of::<u32>() <= Self::MIN_V1_SIZE <= core::mem::size_of::<Self>()`.
/// 3. Every field in `Self` located at byte offset `>= Self::MIN_V1_SIZE` is valid when all-bits-zero
///    (e.g. integers, `ScheduleLabelId`, raw pointers `*const T` / `*mut T`, or `Option<extern "C" fn(...)>`,
///    never non-`Option` bare `fn` pointers or references).
/// 4. Any non-nullable fields inside `0..Self::MIN_V1_SIZE` (such as bare `extern "C" fn` pointers
///    that C callers might erroneously pass as `NULL`) are verified in [`Self::validate_raw_slot`]
///    before `MaybeUninit::assume_init()` is called.
pub unsafe trait ExtensibleStruct: Copy + 'static {
    /// Minimum valid `struct_size` in bytes (the frozen base layout size).
    const MIN_V1_SIZE: usize;

    /// Validates raw slot bytes (e.g., checking non-nullable `fn` pointer offsets) before
    /// `MaybeUninit::assume_init()` materializes `Self`.
    ///
    /// # Safety
    /// `slot_ptr` must point to a `MaybeUninit<Self>` buffer whose first `copy_len >= Self::MIN_V1_SIZE`
    /// bytes were copied from caller memory and whose remaining trailing bytes are zero-initialized.
    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let _ = slot_ptr;
        FfiResult::OK
    }
}

/// Reads a variable-length `#[repr(C)]` descriptor or API table across the FFI boundary using
/// Vulkan / Godot GDExtension `struct_size` rules:
/// - Returns `Err(FfiResult::NULL_POINTER)` if `ptr` is null.
/// - Returns `Err(FfiResult::INVALID_VERSION)` if `struct_size < T::MIN_V1_SIZE`.
/// - Copies `min(struct_size, size_of::<T>())` bytes into a zero-initialized `MaybeUninit<T>`,
///   zero-filling newer trailing fields when `struct_size < size_of::<T>()` and ignoring unknown
///   future trailing bytes when `struct_size > size_of::<T>()`.
/// - Runs [`ExtensibleStruct::validate_raw_slot`] before `assume_init()` so null function pointers
///   from foreign C callers return `Err(FfiResult::NULL_POINTER)` without Rust niche UB.
///
/// # Safety
/// If `ptr` is non-null and `struct_size >= T::MIN_V1_SIZE`, `ptr` must be readable for at least
/// `min(struct_size, core::mem::size_of::<T>())` bytes with valid field values for `T`'s prefix.
#[inline]
pub unsafe fn read_extensible_struct<T: ExtensibleStruct>(
    ptr: *const u8,
    struct_size: usize,
) -> Result<T, FfiResult> {
    if ptr.is_null() {
        return Err(FfiResult::NULL_POINTER);
    }
    if struct_size < T::MIN_V1_SIZE {
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
    // SAFETY: `slot` holds `copy_len >= T::MIN_V1_SIZE` copied prefix bytes and zeroed trailing bytes.
    let status = unsafe { T::validate_raw_slot(slot.as_ptr()) };
    if status.is_err() {
        return Err(status);
    }
    // SAFETY: Prefix `0..copy_len` is valid per caller, trailing `copy_len..size_of::<T>()` is
    // zero-valid per `T: ExtensibleStruct`, and `validate_raw_slot` verified non-null fn slots.
    Ok(unsafe { slot.assume_init() })
}

/// Reads an extensible `#[repr(C)]` descriptor starting with `struct_size: u32` at offset `0`.
///
/// # Safety
/// If `ptr` is non-null, it must be readable for at least 4 bytes to inspect `struct_size`, and
/// if `struct_size >= T::MIN_V1_SIZE`, readable for `min(struct_size, size_of::<T>())` bytes.
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
