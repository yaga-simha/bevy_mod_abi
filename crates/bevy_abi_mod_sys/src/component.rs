//! Runtime dynamic component descriptors and registration callbacks.

use crate::{
    extensible::ExtensibleStruct,
    primitives::{ComponentUuid, FfiResult},
};

/// Function pointer callback invoked by the host to drop resources of a dynamic component instance.
///
/// For POD (Plain Old Data) components that do not own heap allocations or external handles,
/// this callback is unnecessary and should be set to `None` (`NULL` in C).
///
/// # Safety
/// - `ptr` is a non-null, properly aligned pointer to an initialized instance of the component.
/// - The callback must contain any internal panics; unwinding across the FFI boundary is undefined behavior.
pub type ComponentDropFn = unsafe extern "C" fn(ptr: *mut core::ffi::c_void);

/// Descriptor used by dynamic mods to declare and register dynamic components with the host engine at runtime.
///
/// Provides complete memory layout specification (`size`, `align`), identification (`name`, `uuid`),
/// and optional lifecycle hooks (`drop_fn`, `vtable_ptr`) across the `extern "C"` boundary without
/// requiring compile-time host knowledge.
///
/// # Binary Layout (64-bit Architecture)
/// - `struct_size`: offset 0, size 4 (4 bytes padding follows)
/// - `name`: offset 8, size 8
/// - `uuid`: offset 16, size 16
/// - `size`: offset 32, size 8
/// - `align`: offset 40, size 8
/// - `drop_fn`: offset 48, size 8
/// - `vtable_ptr`: offset 56, size 8
/// - Total size: 64 bytes, alignment: 8 bytes.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{ModComponentDescriptor, ComponentUuid, FfiResult};
///
/// let desc = ModComponentDescriptor::new(
///     c"Health".as_ptr(),
///     ComponentUuid::from_name("Health"),
///     4, // size_of::<u32>()
///     4, // align_of::<u32>()
/// );
///
/// assert!(desc.is_valid());
/// assert_eq!(desc.validate(), FfiResult::OK);
/// assert!(!desc.is_zst());
/// assert!(desc.is_pod());
/// ```
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ModComponentDescriptor {
    /// Size of this struct in bytes (`core::mem::size_of::<ModComponentDescriptor>() as u32`).
    ///
    /// Enables forward and backward ABI compatibility negotiation between host and mods.
    pub struct_size: u32,

    /// Null-terminated UTF-8 C string identifying the component name (e.g., `c"CustomTag"`).
    pub name: *const core::ffi::c_char,

    /// Deterministic 128-bit component UUID.
    pub uuid: ComponentUuid,

    /// Byte size (`core::mem::size_of::<T>()`) of the component.
    ///
    /// Must be a multiple of `align` (or `0` for zero-sized types).
    pub size: usize,

    /// Memory alignment (`core::mem::align_of::<T>()`) in bytes.
    ///
    /// Must be a non-zero power of two.
    pub align: usize,

    /// Optional lifecycle drop callback.
    ///
    /// If `Some`, invoked when an entity possessing this component is despawned or the component
    /// is removed from an entity. If `None`, the component is treated as POD.
    pub drop_fn: Option<ComponentDropFn>,

    /// Optional static type-erased VTable pointer for foreign accessor functions.
    ///
    /// May be null if the component is accessed purely via direct contiguous column pointers.
    pub vtable_ptr: *const core::ffi::c_void,
}

/// Ergonomic type alias for [`ModComponentDescriptor`].
pub type DynamicComponentDescriptor = ModComponentDescriptor;

impl ModComponentDescriptor {
    /// Minimum valid base v0.1 descriptor size (`64` bytes on 64-bit, `40` bytes on 32-bit).
    ///
    /// If future minor ABI revisions append optional fields to `ModComponentDescriptor`, descriptors
    /// from v0.1 mods with `struct_size >= MIN_V1_SIZE` remain valid and have trailing fields
    /// zero-filled via [`crate::read_extensible_struct`].
    pub const MIN_V1_SIZE: usize =
        core::mem::offset_of!(Self, vtable_ptr) + core::mem::size_of::<*const core::ffi::c_void>();

    /// Constructs a basic descriptor for a Plain Old Data (POD) component with no custom drop glue.
    #[inline]
    pub const fn new(
        name: *const core::ffi::c_char,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name,
            uuid,
            size,
            align,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        }
    }

    /// Attaches an optional lifecycle drop hook to this descriptor.
    #[inline]
    pub const fn with_drop(mut self, drop_fn: ComponentDropFn) -> Self {
        self.drop_fn = Some(drop_fn);
        self
    }

    /// Attaches an optional static VTable pointer to this descriptor.
    #[inline]
    pub const fn with_vtable(mut self, vtable_ptr: *const core::ffi::c_void) -> Self {
        self.vtable_ptr = vtable_ptr;
        self
    }

    /// Validates the memory layout and pointer integrity of this descriptor.
    ///
    /// Returns `true` if and only if all of the following conditions are met:
    /// 1. `struct_size` is at least [`ModComponentDescriptor::MIN_V1_SIZE`] (ABI compatibility).
    /// 2. `name` is non-null.
    /// 3. `align` is a non-zero power of two.
    /// 4. `size` is an integer multiple of `align` (`size % align == 0`), which also holds for ZSTs (`size == 0`).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Validates the descriptor and returns a specific [`FfiResult`] error code on failure.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if self.name.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.align.is_power_of_two() || !self.size.is_multiple_of(self.align) {
            return FfiResult::INVALID_COMPONENT;
        }
        FfiResult::OK
    }

    /// Returns `true` if this component is a Zero-Sized Type (ZST).
    #[inline]
    pub const fn is_zst(&self) -> bool {
        self.size == 0
    }

    /// Returns `true` if this component does not require a custom drop callback.
    #[inline]
    pub const fn is_pod(&self) -> bool {
        self.drop_fn.is_none()
    }

    /// Returns the verified [`core::alloc::Layout`] for this component descriptor,
    /// or `None` if the alignment is invalid or the size overflows address space bounds.
    #[inline]
    pub fn layout(&self) -> Option<core::alloc::Layout> {
        if !self.align.is_power_of_two() || !self.size.is_multiple_of(self.align) {
            return None;
        }
        core::alloc::Layout::from_size_align(self.size, self.align).ok()
    }

    /// Borrows the component name as a C string reference if `self.name` is non-null.
    ///
    /// # Safety
    /// Caller guarantees that if `self.name` is non-null, it points to a valid null-terminated C string
    /// that remains valid for lifetime `'a`.
    #[inline]
    pub unsafe fn name_cstr<'a>(&self) -> Option<&'a core::ffi::CStr> {
        if self.name.is_null() {
            None
        } else {
            // SAFETY: Caller upholds valid null-terminated C string pointer for lifetime `'a`.
            Some(unsafe { core::ffi::CStr::from_ptr(self.name) })
        }
    }
}

const _: () = {
    assert!(ModComponentDescriptor::MIN_V1_SIZE <= core::mem::size_of::<ModComponentDescriptor>());
    #[cfg(target_pointer_width = "64")]
    assert!(ModComponentDescriptor::MIN_V1_SIZE == 64);
    #[cfg(target_pointer_width = "32")]
    assert!(ModComponentDescriptor::MIN_V1_SIZE == 40);
};

// SAFETY: `ModComponentDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and all
// fields (`drop_fn: Option<...>`, `vtable_ptr: *const c_void`, etc.) are valid when zero-filled.
unsafe impl ExtensibleStruct for ModComponentDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;
}

/// Dynamic component registration function signature exposed in [`crate::HostApi`] and via `get_proc_address`.
///
/// # Safety
/// - `host_context` must point to valid host engine context provided during mod initialization.
/// - `descriptor` must point to a valid, initialized [`ModComponentDescriptor`].
pub type RegisterComponentFn = unsafe extern "C" fn(
    host_context: *mut core::ffi::c_void,
    descriptor: *const ModComponentDescriptor,
) -> FfiResult;
