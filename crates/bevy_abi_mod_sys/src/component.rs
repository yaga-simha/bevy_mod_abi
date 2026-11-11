//! Component registration descriptors (`ComponentDesc`), flags, destructors, and `VTableHeader`.

use core::ffi::{c_char, c_void};

use crate::{
    extensible::ExtensibleStruct,
    primitives::{ComponentUuid, FfiResult},
    version::{ABI_VERSION, abi_version_is_compatible},
};

/// Callback invoked by the host to drop guest-owned resources inside a dynamic component instance
/// (`typedef void (*BamComponentDropFn)(void* ptr);`).
///
/// # Safety
/// - `ptr` must be a non-null, properly aligned pointer to an initialized instance of the component.
/// - The callback must never unwind across the `extern "C"` boundary.
pub type ComponentDropFn = unsafe extern "C" fn(ptr: *mut c_void);

/// Register component with `StorageType::SparseSet` instead of `StorageType::Table`.
pub const BAM_COMPONENT_FLAG_SPARSE_SET: u32 = 1 << 0;

/// Mark component as immutable (reject `WRITE`, `OPTIONAL_WRITE`, and `RESOURCE_WRITE` accesses
/// with [`FfiResult::PERMISSION_DENIED`]).
pub const BAM_COMPONENT_FLAG_IMMUTABLE: u32 = 1 << 1;

/// Bitmask of all recognized [`ComponentDesc::flags`] bits in ABI `0.2`.
pub const BAM_COMPONENT_KNOWN_FLAGS: u32 =
    BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE;

/// Runtime component registration descriptor (`typedef struct BamComponentDesc BamComponentDesc;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 64` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 44` bytes, `4`-byte alignment, zero implicit padding.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{ComponentDesc, ComponentUuid, FfiResult, BAM_COMPONENT_FLAG_SPARSE_SET};
///
/// let uuid = ComponentUuid::from_parts("combat_mod::Health", 4, 4, 0);
/// let desc = ComponentDesc::new(c"combat_mod::Health".as_ptr(), uuid, 4, 4)
///     .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET);
/// assert_eq!(desc.validate(), FfiResult::OK);
/// assert!(desc.is_sparse_set());
/// ```
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Bitmask of `BAM_COMPONENT_FLAG_*` (`offset 4`).
    pub flags: u32,
    /// Canonical NUL-terminated UTF-8 component name, e.g. `c"my_mod::Health"` (`offset 8`).
    pub name: *const c_char,
    /// Deterministic 128-bit component UUID (`offset 16` on 64-bit / `12` on 32-bit).
    pub uuid: ComponentUuid,
    /// Component byte size (`size_of::<T>()`, `offset 32` on 64-bit / `28` on 32-bit).
    pub size: usize,
    /// Component byte alignment (`align_of::<T>()`, `offset 40` on 64-bit / `32` on 32-bit).
    pub align: usize,
    /// Optional guest destructor callback (`offset 48` on 64-bit / `36` on 32-bit).
    pub drop_fn: Option<ComponentDropFn>,
    /// Optional `VTableHeader`-prefixed static VTable pointer (`offset 56` on 64-bit / `40` on 32-bit).
    pub vtable_ptr: *const c_void,
}

impl ComponentDesc {
    /// Frozen minimum valid `ComponentDesc` size in ABI `0.2` (`64` on 64-bit, `44` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, vtable_ptr) + core::mem::size_of::<*const c_void>();

    /// Constructs a Plain Old Data component descriptor with `flags = 0`, `drop_fn = None`, and
    /// `vtable_ptr = null`.
    #[inline]
    pub const fn new(name: *const c_char, uuid: ComponentUuid, size: usize, align: usize) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name,
            uuid,
            size,
            align,
            drop_fn: None,
            vtable_ptr: core::ptr::null(),
        }
    }

    /// Sets `BAM_COMPONENT_FLAG_*` bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Attaches an optional guest destructor callback.
    #[inline]
    pub const fn with_drop(mut self, drop_fn: ComponentDropFn) -> Self {
        self.drop_fn = Some(drop_fn);
        self
    }

    /// Attaches an optional [`VTableHeader`]-prefixed static VTable pointer.
    #[inline]
    pub const fn with_vtable(mut self, vtable_ptr: *const c_void) -> Self {
        self.vtable_ptr = vtable_ptr;
        self
    }

    /// Returns `true` if `BAM_COMPONENT_FLAG_SPARSE_SET` is set.
    #[inline]
    pub const fn is_sparse_set(&self) -> bool {
        (self.flags & BAM_COMPONENT_FLAG_SPARSE_SET) != 0
    }

    /// Returns `true` if `BAM_COMPONENT_FLAG_IMMUTABLE` is set.
    #[inline]
    pub const fn is_immutable(&self) -> bool {
        (self.flags & BAM_COMPONENT_FLAG_IMMUTABLE) != 0
    }

    /// Returns `true` if `self.size == 0` (marker component).
    #[inline]
    pub const fn is_zst(&self) -> bool {
        self.size == 0
    }

    /// Returns `true` if no custom `drop_fn` is attached.
    #[inline]
    pub const fn is_pod(&self) -> bool {
        self.drop_fn.is_none()
    }

    /// Returns the verified [`core::alloc::Layout`] for `(self.size, self.align)`, or `None` if
    /// alignment is not a power of two, `size % align != 0`, or size exceeds `isize::MAX`.
    #[inline]
    pub fn layout(&self) -> Option<core::alloc::Layout> {
        if !self.align.is_power_of_two() || !self.size.is_multiple_of(self.align) {
            return None;
        }
        core::alloc::Layout::from_size_align(self.size, self.align).ok()
    }

    /// Validates `struct_size`, known `flags`, non-null `name`, and `(size, align)` invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_COMPONENT_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.name.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.layout().is_none() {
            return FfiResult::INVALID_ARGUMENT;
        }
        FfiResult::OK
    }

    /// Returns `true` if [`Self::validate`] succeeds (`FfiResult::OK`).
    #[inline]
    pub fn is_valid(&self) -> bool {
        self.validate().is_ok()
    }

    /// Borrows `self.name` as `&CStr` when non-null.
    ///
    /// # Safety
    /// If `self.name` is non-null, it must point to a valid NUL-terminated C string that remains
    /// immutable for lifetime `'a`.
    #[inline]
    pub unsafe fn name_cstr<'a>(&self) -> Option<&'a core::ffi::CStr> {
        if self.name.is_null() {
            None
        } else {
            // SAFETY: Caller guarantees `self.name` points to a NUL-terminated C string for `'a`.
            Some(unsafe { core::ffi::CStr::from_ptr(self.name) })
        }
    }
}

const _: () = {
    assert!(ComponentDesc::MIN_V0_2_SIZE <= core::mem::size_of::<ComponentDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(ComponentDesc::MIN_V0_2_SIZE == 64);
        assert!(core::mem::size_of::<ComponentDesc>() == 64);
        assert!(core::mem::align_of::<ComponentDesc>() == 8);
        assert!(core::mem::offset_of!(ComponentDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(ComponentDesc, flags) == 4);
        assert!(core::mem::offset_of!(ComponentDesc, name) == 8);
        assert!(core::mem::offset_of!(ComponentDesc, uuid) == 16);
        assert!(core::mem::offset_of!(ComponentDesc, size) == 32);
        assert!(core::mem::offset_of!(ComponentDesc, align) == 40);
        assert!(core::mem::offset_of!(ComponentDesc, drop_fn) == 48);
        assert!(core::mem::offset_of!(ComponentDesc, vtable_ptr) == 56);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(ComponentDesc::MIN_V0_2_SIZE == 44);
        assert!(core::mem::size_of::<ComponentDesc>() == 44);
        assert!(core::mem::align_of::<ComponentDesc>() == 4);
        assert!(core::mem::offset_of!(ComponentDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(ComponentDesc, flags) == 4);
        assert!(core::mem::offset_of!(ComponentDesc, name) == 8);
        assert!(core::mem::offset_of!(ComponentDesc, uuid) == 12);
        assert!(core::mem::offset_of!(ComponentDesc, size) == 28);
        assert!(core::mem::offset_of!(ComponentDesc, align) == 32);
        assert!(core::mem::offset_of!(ComponentDesc, drop_fn) == 36);
        assert!(core::mem::offset_of!(ComponentDesc, vtable_ptr) == 40);
    }
};

// SAFETY: `ComponentDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields (`drop_fn: Option<...>`, pointers, integers, `[u8; 16]`) are valid when zero-filled.
unsafe impl ExtensibleStruct for ComponentDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}

/// Self-describing identity and layout header placed at byte offset `0` of every generated
/// `<Struct>VTable` (`typedef struct BamVTableHeader BamVTableHeader;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 40` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 32` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct VTableHeader {
    /// Total byte size of the enclosing `<Struct>VTable` (`offset 0`).
    pub struct_size: u32,
    /// Packed ABI version (`ABI_VERSION`, `offset 4`).
    pub abi_version: u32,
    /// Component [`ComponentUuid`] (`offset 8`, 16 bytes).
    pub uuid: ComponentUuid,
    /// Host component `size_of::<Struct>()` (`offset 24`).
    pub size: usize,
    /// Host component `align_of::<Struct>()` (`offset 32` on 64-bit / `28` on 32-bit).
    pub align: usize,
}

impl VTableHeader {
    /// Frozen minimum valid `VTableHeader` size in ABI `0.2` (`40` on 64-bit, `32` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, align) + core::mem::size_of::<usize>();

    /// Constructs a `VTableHeader` for `VTable` type `V` and host component type `T`.
    #[inline]
    pub const fn new<V, T>(uuid: ComponentUuid) -> Self {
        Self {
            struct_size: core::mem::size_of::<V>() as u32,
            abi_version: ABI_VERSION,
            uuid,
            size: core::mem::size_of::<T>(),
            align: core::mem::align_of::<T>(),
        }
    }

    /// Constructs a `VTableHeader` from explicit layout parts.
    #[inline]
    pub const fn from_raw_parts(
        vtable_struct_size: u32,
        uuid: ComponentUuid,
        size: usize,
        align: usize,
    ) -> Self {
        Self {
            struct_size: vtable_struct_size,
            abi_version: ABI_VERSION,
            uuid,
            size,
            align,
        }
    }

    /// Validates that `struct_size` covers `expected_min_vtable_size`, `abi_version` is compatible
    /// with [`ABI_VERSION`], and `uuid` matches `expected_uuid`.
    #[inline]
    pub fn validate(
        &self,
        expected_min_vtable_size: usize,
        expected_uuid: ComponentUuid,
    ) -> FfiResult {
        let required = if expected_min_vtable_size > Self::MIN_V0_2_SIZE {
            expected_min_vtable_size
        } else {
            Self::MIN_V0_2_SIZE
        };
        if (self.struct_size as usize) < required {
            return FfiResult::INVALID_VERSION;
        }
        if !abi_version_is_compatible(self.abi_version, ABI_VERSION) {
            return FfiResult::INVALID_VERSION;
        }
        if self.uuid != expected_uuid {
            return FfiResult::TYPE_MISMATCH;
        }
        FfiResult::OK
    }
}

const _: () = {
    assert!(VTableHeader::MIN_V0_2_SIZE <= core::mem::size_of::<VTableHeader>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(VTableHeader::MIN_V0_2_SIZE == 40);
        assert!(core::mem::size_of::<VTableHeader>() == 40);
        assert!(core::mem::align_of::<VTableHeader>() == 8);
        assert!(core::mem::offset_of!(VTableHeader, struct_size) == 0);
        assert!(core::mem::offset_of!(VTableHeader, abi_version) == 4);
        assert!(core::mem::offset_of!(VTableHeader, uuid) == 8);
        assert!(core::mem::offset_of!(VTableHeader, size) == 24);
        assert!(core::mem::offset_of!(VTableHeader, align) == 32);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(VTableHeader::MIN_V0_2_SIZE == 32);
        assert!(core::mem::size_of::<VTableHeader>() == 32);
        assert!(core::mem::align_of::<VTableHeader>() == 4);
        assert!(core::mem::offset_of!(VTableHeader, struct_size) == 0);
        assert!(core::mem::offset_of!(VTableHeader, abi_version) == 4);
        assert!(core::mem::offset_of!(VTableHeader, uuid) == 8);
        assert!(core::mem::offset_of!(VTableHeader, size) == 24);
        assert!(core::mem::offset_of!(VTableHeader, align) == 28);
    }
};

// SAFETY: `VTableHeader` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for VTableHeader {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_2_SIZE;
}
