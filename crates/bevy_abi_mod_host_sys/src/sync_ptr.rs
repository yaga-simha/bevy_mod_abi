//! Narrow raw-pointer wrappers isolating `Send` and `Sync` FFI invariants.

use bevy_abi_mod_sys::BamInstance;
use core::ffi::c_void;

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
// any single `GuestSystem` instance across worker threads, matching the C-ABI system contract.
unsafe impl Send for GuestUserData {}

// SAFETY: Shared references to `GuestSystem` never dereference or expose mutable aliasing to
// `GuestUserData`; callbacks only run under exclusive `&mut self` scheduler access.
unsafe impl Sync for GuestUserData {}

/// Opaque per-`World` guest instance pointer returned by `bevy_abi_mod_init` and passed back to
/// `bevy_abi_mod_shutdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestInstancePtr(*mut BamInstance);

impl GuestInstancePtr {
    /// Null guest instance pointer constant.
    pub const NULL: Self = Self(core::ptr::null_mut());

    /// Wraps an opaque `*mut BamInstance` handle.
    #[inline]
    pub const fn new(ptr: *mut BamInstance) -> Self {
        Self(ptr)
    }

    /// Returns the underlying `*mut BamInstance` pointer.
    #[inline]
    pub const fn as_ptr(self) -> *mut BamInstance {
        self.0
    }
}

// SAFETY: The host never dereferences `GuestInstancePtr`; it is stored inside `LoadedMod` and
// passed back once to `bevy_abi_mod_shutdown` under exclusive `&mut World` teardown.
unsafe impl Send for GuestInstancePtr {}

// SAFETY: Shared references to `LoadedMod` never expose or dereference `GuestInstancePtr`.
unsafe impl Sync for GuestInstancePtr {}

/// Narrow wrapper around a foreign `extern "C"` function pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFnPtr<F: Copy>(pub F);

// SAFETY: Foreign `extern "C"` code pointers refer to immutable executable instructions and carry
// no thread-local ownership state; validity across `dlclose` is guarded by `is_active` / eviction.
unsafe impl<F: Copy> Send for GuestFnPtr<F> {}

// SAFETY: Code pointers are immutable values safe to copy and inspect from any thread.
unsafe impl<F: Copy> Sync for GuestFnPtr<F> {}
