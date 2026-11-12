//! Narrow raw-pointer and aligned byte-buffer wrappers isolating `Send` and `Sync` invariants.

use bevy_abi_mod_sys::FfiResult;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use std::alloc::{Layout, alloc, dealloc, handle_alloc_error};

/// Resolved `extern "C"` symbol or VTable address stored in [`crate::ModRegistry`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SymbolAddress(*const c_void);

impl SymbolAddress {
    /// Null symbol address constant.
    pub const NULL: Self = Self(core::ptr::null());

    /// Wraps a raw read-only symbol or VTable address.
    #[inline]
    pub const fn new(ptr: *const c_void) -> Self {
        Self(ptr)
    }

    /// Returns the underlying raw `*const c_void` address.
    #[inline]
    pub const fn as_ptr(self) -> *const c_void {
        self.0
    }

    /// Returns `true` when the wrapped address is null.
    #[inline]
    pub fn is_null(self) -> bool {
        self.0.is_null()
    }
}

// SAFETY: `SymbolAddress` points to static read-only `extern "C"` function tables or functions
// whose owner lifetime is tracked and evicted prior to library unload.
unsafe impl Send for SymbolAddress {}

// SAFETY: Concurrent reads of immutable `extern "C"` function tables or symbol pointers across
// worker threads involve no shared interior mutability.
unsafe impl Sync for SymbolAddress {}

/// Opaque guest `user_data` context pointer passed back to C-ABI callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestUserData(*mut c_void);

impl GuestUserData {
    /// Null guest context pointer constant.
    pub const NULL: Self = Self(core::ptr::null_mut());

    /// Wraps an opaque guest context pointer.
    #[inline]
    pub const fn new(ptr: *mut c_void) -> Self {
        Self(ptr)
    }

    /// Returns the underlying `*mut c_void` pointer.
    #[inline]
    pub const fn as_ptr(self) -> *mut c_void {
        self.0
    }
}

// SAFETY: The host never dereferences `GuestUserData`; Bevy's scheduler serializes execution of
// any single `DynamicCSystem` instance across worker threads, matching the C-ABI system contract.
unsafe impl Send for GuestUserData {}

// SAFETY: Shared references to `DynamicCSystem` never dereference or expose mutable aliasing to
// `GuestUserData`; callbacks only run under exclusive `&mut self` scheduler access.
unsafe impl Sync for GuestUserData {}

/// Narrow wrapper around a foreign `extern "C"` function pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFnPtr<F: Copy>(pub F);

// SAFETY: Foreign `extern "C"` code pointers refer to immutable executable instructions and carry
// no thread-local ownership state; validity across `dlclose` is guarded by `is_active` / eviction.
unsafe impl<F: Copy> Send for GuestFnPtr<F> {}

// SAFETY: Code pointers are immutable values safe to copy and inspect from any thread.
unsafe impl<F: Copy> Sync for GuestFnPtr<F> {}

/// Narrow wrapper for retired heap snapshot pointers retained behind a mutex.
pub(crate) struct RetiredSnapshotPtr<T>(*mut T);

impl<T> RetiredSnapshotPtr<T> {
    #[inline]
    pub(crate) const fn new(ptr: *mut T) -> Self {
        Self(ptr)
    }
}

impl<T> Drop for RetiredSnapshotPtr<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: `RetiredSnapshotPtr::new` is constructed exclusively from `Box::into_raw`
            // on a heap-allocated snapshot after swapping it out of `DROP_SLOT_SNAPSHOTS`, and is
            // dropped at most once if the owning container is destroyed.
            drop(unsafe { Box::from_raw(self.0) });
        }
    }
}

// SAFETY: `RetiredSnapshotPtr<T>` owns a heap allocation of `T: Send + Sync` that is only mutated
// or reclaimed while holding the enclosing mutex.
unsafe impl<T: Send + Sync> Send for RetiredSnapshotPtr<T> {}

// SAFETY: Concurrent shared references to `RetiredSnapshotPtr<T>` only read immutable `T: Sync`
// snapshot data.
unsafe impl<T: Send + Sync> Sync for RetiredSnapshotPtr<T> {}

/// Heap-allocated `MaybeUninit<u8>` buffer aligned to a dynamic or static component's layout.
///
/// Copies raw bytes via `MaybeUninit<u8>` without ever forming `&[u8]`, preserving sound
/// provenance and avoiding undefined behavior when C or Rust structs contain uninitialized
/// padding bytes.
pub(crate) struct AlignedBytes {
    ptr: *mut MaybeUninit<u8>,
    layout: Layout,
}

impl AlignedBytes {
    /// Allocates a buffer of `(size, align)` and copies `size` raw `MaybeUninit<u8>` bytes from `src`.
    ///
    /// # Safety
    /// If `size > 0`, `src` must be non-null and readable for `size` bytes (individual padding
    /// bytes may be uninitialized).
    pub(crate) unsafe fn copy_from_guest(
        src: *const u8,
        size: usize,
        align: usize,
    ) -> Result<Self, FfiResult> {
        if align == 0 || !align.is_power_of_two() {
            return Err(FfiResult::INVALID_ARGUMENT);
        }
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return Err(FfiResult::INVALID_ARGUMENT);
        };

        if size == 0 {
            return Ok(Self {
                ptr: core::ptr::without_provenance_mut(align),
                layout,
            });
        }

        if src.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }

        // SAFETY: `layout` has `size > 0` and valid power-of-two `align` verified above.
        let raw = unsafe { alloc(layout) }.cast::<MaybeUninit<u8>>();
        if raw.is_null() {
            handle_alloc_error(layout);
        }

        // SAFETY: Caller guarantees `src` is readable for `size` `MaybeUninit<u8>` bytes; `raw` is
        // freshly allocated and non-overlapping with `src`. Copying as `MaybeUninit<u8>` avoids
        // forming `&[u8]` over uninitialized struct padding bytes.
        unsafe {
            core::ptr::copy_nonoverlapping(src.cast::<MaybeUninit<u8>>(), raw, size);
        }

        Ok(Self { ptr: raw, layout })
    }

    /// Returns a const byte pointer aligned to `self.layout.align()`.
    #[inline]
    pub(crate) fn as_u8_ptr(&self) -> *const u8 {
        self.ptr.cast::<u8>().cast_const()
    }

    /// Returns a mutable byte pointer aligned to `self.layout.align()` for `OwningPtr` transfer.
    ///
    /// Requires `&mut self` to preserve exclusive access semantics when exposing a raw mutable pointer.
    #[inline]
    #[allow(clippy::needless_pass_by_ref_mut)]
    pub(crate) fn as_mut_u8_ptr(&mut self) -> *mut u8 {
        self.ptr.cast::<u8>()
    }
}

impl Drop for AlignedBytes {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: `self.ptr` was allocated via `std::alloc::alloc(self.layout)` with `size > 0`.
            unsafe {
                dealloc(self.ptr.cast::<u8>(), self.layout);
            }
        }
    }
}

// SAFETY: `AlignedBytes` uniquely owns its heap allocation of raw component bytes and does not
// alias any thread-local storage.
unsafe impl Send for AlignedBytes {}

// SAFETY: Shared references `&AlignedBytes` expose only read-only byte pointers.
unsafe impl Sync for AlignedBytes {}
