//! Component registration descriptors (`ComponentDesc`), flags, destructors, clone hooks, and `VTableHeader`.

use core::ffi::{c_char, c_void};

use crate::{
    access::ScheduleId,
    extensible::ExtensibleStruct,
    primitives::{BamComponent, ComponentUuid, FfiEntity, FfiResult, StrView},
    traits::AbiPod,
    version::{ABI_VERSION, abi_version_is_compatible},
};

/// Callback invoked by the host to drop guest-owned resources inside a dynamic component instance
/// (`typedef void (*BamComponentDropFn)(void* ptr);`).
///
/// # Safety
/// - `ptr` must be a non-null, properly aligned pointer to an initialized instance of the component.
/// - The callback must never unwind across the `extern "C"` boundary.
pub type ComponentDropFn = unsafe extern "C" fn(ptr: *mut c_void);

/// Lifecycle hook callback (`on_add` / `on_insert` / `on_discard` / `on_remove` / `on_despawn`)
/// invoked by the host when a dynamic component transitions on an entity
/// (`typedef void (*BamComponentHookFn)(BamEntity entity, const void* data_ptr, void* user_data);`).
///
/// # Safety
/// - `data_ptr` points to the entity's component payload while present in Bevy storage (valid only
///   for the synchronous duration of the hook call).
/// - `user_data` is the opaque pointer registered in [`ComponentDesc::hook_user_data`].
/// - The callback must never unwind across the `extern "C"` boundary.
pub type ComponentHookFn =
    unsafe extern "C" fn(entity: FfiEntity, data_ptr: *const c_void, user_data: *mut c_void);

/// Custom component clone callback invoked by Bevy's `EntityCloner` when cloning a dynamic component
/// (`typedef BamResult (*BamComponentCloneFn)(const void* src, void* dst, void* user_data);`).
///
/// # Safety
/// - `src` points to an initialized source component payload of `(size, align)` valid for the call.
/// - `dst` points to uninitialized writable storage of `(size, align)` to be initialized on `OK`.
/// - `user_data` is the opaque pointer registered in [`ComponentDesc::hook_user_data`].
/// - The callback must never unwind across the `extern "C"` boundary.
pub type ComponentCloneFn =
    unsafe extern "C" fn(src: *const c_void, dst: *mut c_void, user_data: *mut c_void) -> FfiResult;

/// Register component with `StorageType::SparseSet` instead of `StorageType::Table`.
pub const BAM_COMPONENT_FLAG_SPARSE_SET: u32 = 1 << 0;

/// Mark component as immutable (reject `WRITE`, `OPTIONAL_WRITE`, and `RESOURCE_WRITE` accesses
/// with [`FfiResult::PERMISSION_DENIED`]).
pub const BAM_COMPONENT_FLAG_IMMUTABLE: u32 = 1 << 1;

/// Enable per-table Bevy 0.20 `summary_tick` acceleration for `Changed<T>` / `Added<T>` scans
/// (valid only on `Table`-stored components; rejected when combined with [`BAM_COMPONENT_FLAG_SPARSE_SET`]).
pub const BAM_COMPONENT_FLAG_SUMMARY_TICK: u32 = 1 << 2;

/// Bitmask of all recognized [`ComponentDesc::flags`] bits in ABI `0.2`.
pub const BAM_COMPONENT_KNOWN_FLAGS: u32 =
    BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_IMMUTABLE | BAM_COMPONENT_FLAG_SUMMARY_TICK;

/// Runtime component registration descriptor (`typedef struct BamComponentDesc BamComponentDesc;`).
///
/// Layout:
/// - 64-bit: `MIN_V0_2_SIZE = 64` bytes (`size_of = 176` bytes with v0.3 schema fingerprint,
///   fields, and required-component custom initializers), `8`-byte alignment, zero implicit padding.
/// - 32-bit: `MIN_V0_2_SIZE = 44` bytes (`size_of = 104` bytes), `align = align_of::<u64>()` (`4` on
///   `i686`, `8` on `wasm32`), zero implicit padding on every target.
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
    /// Optional guest `on_add` lifecycle hook (`offset 64` on 64-bit / `44` on 32-bit).
    pub on_add: Option<ComponentHookFn>,
    /// Optional guest `on_remove` lifecycle hook (`offset 72` on 64-bit / `48` on 32-bit).
    pub on_remove: Option<ComponentHookFn>,
    /// Opaque context pointer passed to lifecycle hooks and `clone_fn` (`offset 80` on 64-bit / `52` on 32-bit).
    pub hook_user_data: *mut c_void,
    /// Optional guest `on_insert` lifecycle hook (`offset 88` on 64-bit / `56` on 32-bit).
    pub on_insert: Option<ComponentHookFn>,
    /// Optional guest `on_discard` (replacement) lifecycle hook (`offset 96` on 64-bit / `60` on 32-bit).
    pub on_discard: Option<ComponentHookFn>,
    /// Optional guest `on_despawn` lifecycle hook (`offset 104` on 64-bit / `64` on 32-bit).
    pub on_despawn: Option<ComponentHookFn>,
    /// Optional custom `EntityCloner` callback (`offset 112` on 64-bit / `68` on 32-bit).
    pub clone_fn: Option<ComponentCloneFn>,
    /// Optional array of required [`BamComponent`] handles (`required_components[0..required_component_count]`, `offset 120` on 64-bit / `72` on 32-bit).
    pub required_components: *const BamComponent,
    /// Number of entries in `required_components` (`offset 128` on 64-bit / `76` on 32-bit).
    pub required_component_count: usize,
    /// Deterministic 64-bit FNV-1a schema fingerprint (`offset 136` on 64-bit / `80` on 32-bit).
    pub schema_fingerprint: u64,
    /// Optional array of static field descriptors (`fields[0..field_count]`, `offset 144` on 64-bit / `88` on 32-bit).
    pub fields: *const BamFieldDesc,
    /// Number of entries in `fields` (`offset 152` on 64-bit / `92` on 32-bit).
    pub field_count: usize,
    /// Optional array of required-component initialization descriptors (`offset 160` on 64-bit / `96` on 32-bit).
    pub required_init_descs: *const BamRequiredComponent,
    /// Number of entries in `required_init_descs` (`offset 168` on 64-bit / `100` on 32-bit).
    pub required_init_desc_count: usize,
}

impl ComponentDesc {
    /// Frozen minimum valid `ComponentDesc` size in ABI `0.2` (`64` on 64-bit, `44` on 32-bit).
    pub const MIN_V0_2_SIZE: usize =
        core::mem::offset_of!(Self, vtable_ptr) + core::mem::size_of::<*const c_void>();

    /// Full ABI `0.3` size of `ComponentDesc` (`176` on 64-bit, `104` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a Plain Old Data component descriptor with `flags = 0`, `drop_fn = None`,
    /// `vtable_ptr = null`, no lifecycle/clone hooks, and no field/required metadata.
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
            on_add: None,
            on_remove: None,
            hook_user_data: core::ptr::null_mut(),
            on_insert: None,
            on_discard: None,
            on_despawn: None,
            clone_fn: None,
            required_components: core::ptr::null(),
            required_component_count: 0,
            schema_fingerprint: 0,
            fields: core::ptr::null(),
            field_count: 0,
            required_init_descs: core::ptr::null(),
            required_init_desc_count: 0,
        }
    }

    /// Returns `true` if `self.struct_size` covers a field of `slot_size` bytes at `slot_offset`.
    #[inline]
    pub const fn covers_slot(&self, slot_offset: usize, slot_size: usize) -> bool {
        match slot_offset.checked_add(slot_size) {
            Some(end) => (self.struct_size as usize) >= end,
            None => false,
        }
    }

    /// Sets `BAM_COMPONENT_FLAG_*` bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Enables `BAM_COMPONENT_FLAG_SUMMARY_TICK` on this descriptor.
    #[inline]
    pub const fn with_summary_tick(mut self) -> Self {
        self.flags |= BAM_COMPONENT_FLAG_SUMMARY_TICK;
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

    /// Attaches an `on_add` lifecycle hook callback.
    #[inline]
    pub const fn with_on_add(mut self, on_add: ComponentHookFn) -> Self {
        self.on_add = Some(on_add);
        self
    }

    /// Attaches an `on_insert` lifecycle hook callback.
    #[inline]
    pub const fn with_on_insert(mut self, on_insert: ComponentHookFn) -> Self {
        self.on_insert = Some(on_insert);
        self
    }

    /// Attaches an `on_discard` lifecycle hook callback.
    #[inline]
    pub const fn with_on_discard(mut self, on_discard: ComponentHookFn) -> Self {
        self.on_discard = Some(on_discard);
        self
    }

    /// Attaches an `on_remove` lifecycle hook callback.
    #[inline]
    pub const fn with_on_remove(mut self, on_remove: ComponentHookFn) -> Self {
        self.on_remove = Some(on_remove);
        self
    }

    /// Attaches an `on_despawn` lifecycle hook callback.
    #[inline]
    pub const fn with_on_despawn(mut self, on_despawn: ComponentHookFn) -> Self {
        self.on_despawn = Some(on_despawn);
        self
    }

    /// Sets the shared `hook_user_data` pointer passed to lifecycle hooks and `clone_fn`.
    #[inline]
    pub const fn with_hook_user_data(mut self, user_data: *mut c_void) -> Self {
        self.hook_user_data = user_data;
        self
    }

    /// Attaches `on_add` and/or `on_remove` lifecycle hooks with shared `user_data`.
    #[inline]
    pub const fn with_hooks(
        mut self,
        on_add: Option<ComponentHookFn>,
        on_remove: Option<ComponentHookFn>,
        user_data: *mut c_void,
    ) -> Self {
        self.on_add = on_add;
        self.on_remove = on_remove;
        self.hook_user_data = user_data;
        self
    }

    /// Attaches a custom `clone_fn` callback for Bevy's `EntityCloner`.
    #[inline]
    pub const fn with_clone_fn(mut self, clone_fn: ComponentCloneFn) -> Self {
        self.clone_fn = Some(clone_fn);
        self
    }

    /// Declares required [`BamComponent`] handles (`required_components[0..count]`).
    #[inline]
    pub const fn with_required_components(
        mut self,
        required_components: *const BamComponent,
        required_component_count: usize,
    ) -> Self {
        self.required_components = required_components;
        self.required_component_count = required_component_count;
        self
    }

    /// Attaches the compile-time 64-bit FNV-1a [`ModComponent::SCHEMA_FINGERPRINT`](crate::ModComponent::SCHEMA_FINGERPRINT).
    #[inline]
    pub const fn with_schema_fingerprint(mut self, schema_fingerprint: u64) -> Self {
        self.schema_fingerprint = schema_fingerprint;
        self
    }

    /// Attaches static field metadata (`fields[0..count]`).
    #[inline]
    pub const fn with_fields(mut self, fields: *const BamFieldDesc, field_count: usize) -> Self {
        self.fields = fields;
        self.field_count = field_count;
        self
    }

    /// Attaches extended required-component initialization descriptors (`descs[0..count]`).
    #[inline]
    pub const fn with_required_init_descs(
        mut self,
        required_init_descs: *const BamRequiredComponent,
        required_init_desc_count: usize,
    ) -> Self {
        self.required_init_descs = required_init_descs;
        self.required_init_desc_count = required_init_desc_count;
        self
    }

    /// Returns `true` if any lifecycle hook (`on_add`, `on_insert`, `on_discard`, `on_remove`, or `on_despawn`) is set.
    #[inline]
    pub const fn has_hooks(&self) -> bool {
        self.on_add.is_some()
            || self.on_insert.is_some()
            || self.on_discard.is_some()
            || self.on_remove.is_some()
            || self.on_despawn.is_some()
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

    /// Returns `true` if `BAM_COMPONENT_FLAG_SUMMARY_TICK` is set.
    #[inline]
    pub const fn is_summary_tick(&self) -> bool {
        (self.flags & BAM_COMPONENT_FLAG_SUMMARY_TICK) != 0
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

    /// Validates `struct_size`, known `flags`, incompatible `SPARSE_SET | SUMMARY_TICK`,
    /// non-null `name` / `required_components`, and `(size, align)` invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_2_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_COMPONENT_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.is_sparse_set() && self.is_summary_tick() {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.name.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.required_component_count > 0 && self.required_components.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.field_count > 0 && self.fields.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if self.required_init_desc_count > 0 && self.required_init_descs.is_null() {
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
    assert!(ComponentDesc::MIN_V0_3_SIZE == core::mem::size_of::<ComponentDesc>());
    #[cfg(target_pointer_width = "64")]
    {
        assert!(ComponentDesc::MIN_V0_2_SIZE == 64);
        assert!(core::mem::size_of::<ComponentDesc>() == 176);
        assert!(core::mem::align_of::<ComponentDesc>() == 8);
        assert!(core::mem::offset_of!(ComponentDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(ComponentDesc, flags) == 4);
        assert!(core::mem::offset_of!(ComponentDesc, name) == 8);
        assert!(core::mem::offset_of!(ComponentDesc, uuid) == 16);
        assert!(core::mem::offset_of!(ComponentDesc, size) == 32);
        assert!(core::mem::offset_of!(ComponentDesc, align) == 40);
        assert!(core::mem::offset_of!(ComponentDesc, drop_fn) == 48);
        assert!(core::mem::offset_of!(ComponentDesc, vtable_ptr) == 56);
        assert!(core::mem::offset_of!(ComponentDesc, on_add) == 64);
        assert!(core::mem::offset_of!(ComponentDesc, on_remove) == 72);
        assert!(core::mem::offset_of!(ComponentDesc, hook_user_data) == 80);
        assert!(core::mem::offset_of!(ComponentDesc, on_insert) == 88);
        assert!(core::mem::offset_of!(ComponentDesc, on_discard) == 96);
        assert!(core::mem::offset_of!(ComponentDesc, on_despawn) == 104);
        assert!(core::mem::offset_of!(ComponentDesc, clone_fn) == 112);
        assert!(core::mem::offset_of!(ComponentDesc, required_components) == 120);
        assert!(core::mem::offset_of!(ComponentDesc, required_component_count) == 128);
        assert!(core::mem::offset_of!(ComponentDesc, schema_fingerprint) == 136);
        assert!(core::mem::offset_of!(ComponentDesc, fields) == 144);
        assert!(core::mem::offset_of!(ComponentDesc, field_count) == 152);
        assert!(core::mem::offset_of!(ComponentDesc, required_init_descs) == 160);
        assert!(core::mem::offset_of!(ComponentDesc, required_init_desc_count) == 168);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(ComponentDesc::MIN_V0_2_SIZE == 44);
        assert!(core::mem::size_of::<ComponentDesc>() == 104);
        assert!(core::mem::align_of::<ComponentDesc>() == core::mem::align_of::<u64>());
        assert!(core::mem::offset_of!(ComponentDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(ComponentDesc, flags) == 4);
        assert!(core::mem::offset_of!(ComponentDesc, name) == 8);
        assert!(core::mem::offset_of!(ComponentDesc, uuid) == 12);
        assert!(core::mem::offset_of!(ComponentDesc, size) == 28);
        assert!(core::mem::offset_of!(ComponentDesc, align) == 32);
        assert!(core::mem::offset_of!(ComponentDesc, drop_fn) == 36);
        assert!(core::mem::offset_of!(ComponentDesc, vtable_ptr) == 40);
        assert!(core::mem::offset_of!(ComponentDesc, on_add) == 44);
        assert!(core::mem::offset_of!(ComponentDesc, on_remove) == 48);
        assert!(core::mem::offset_of!(ComponentDesc, hook_user_data) == 52);
        assert!(core::mem::offset_of!(ComponentDesc, on_insert) == 56);
        assert!(core::mem::offset_of!(ComponentDesc, on_discard) == 60);
        assert!(core::mem::offset_of!(ComponentDesc, on_despawn) == 64);
        assert!(core::mem::offset_of!(ComponentDesc, clone_fn) == 68);
        assert!(core::mem::offset_of!(ComponentDesc, required_components) == 72);
        assert!(core::mem::offset_of!(ComponentDesc, required_component_count) == 76);
        assert!(core::mem::offset_of!(ComponentDesc, schema_fingerprint) == 80);
        assert!(core::mem::offset_of!(ComponentDesc, fields) == 88);
        assert!(core::mem::offset_of!(ComponentDesc, field_count) == 92);
        assert!(core::mem::offset_of!(ComponentDesc, required_init_descs) == 96);
        assert!(core::mem::offset_of!(ComponentDesc, required_init_desc_count) == 100);
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

/// Callback reading an opaque host component/resource at `ptr` into a wire `W: AbiPod` struct at `out_wire`.
///
/// # Safety
/// - `ptr` must point to a valid initialized host instance of the bridged type.
/// - `out_wire` must point to aligned writable `W` storage.
pub type MappedWireGetFn<W> =
    unsafe extern "C" fn(ptr: *const c_void, out_wire: *mut W) -> FfiResult;

/// Callback updating an opaque host component/resource at `ptr` from a wire `W: AbiPod` struct at `in_wire`.
///
/// # Safety
/// - `ptr` must point to a valid exclusively borrowed host instance of the bridged type.
/// - `in_wire` must point to aligned readable `W` storage.
pub type MappedWireSetFn<W> =
    unsafe extern "C" fn(ptr: *mut c_void, in_wire: *const W) -> FfiResult;

/// Generic `#[repr(C)]` VTable bridging an opaque `#[repr(Rust)]` host component or resource
/// (such as Bevy `Transform`, `Visibility`, `Time`, `ButtonInput`, or `State<S>`) to a fixed-layout
/// `W: AbiPod` wire representation.
#[repr(C)]
pub struct MappedWireVTable<W: AbiPod> {
    /// Leading [`VTableHeader`] at byte offset `0`.
    pub header: VTableHeader,
    /// Reads the host value into `*out_wire`.
    pub get: Option<MappedWireGetFn<W>>,
    /// Writes `*in_wire` into the mutable host value (`None` for read-only types like `GlobalTransform`).
    pub set: Option<MappedWireSetFn<W>>,
}

impl<W: AbiPod> Copy for MappedWireVTable<W> {}

impl<W: AbiPod> Clone for MappedWireVTable<W> {
    #[inline]
    fn clone(&self) -> Self {
        *self
    }
}

/// Callback borrowing a UTF-8 [`StrView`] from an opaque host string component/resource at `ptr`.
///
/// # Safety
/// `ptr` must point to a valid initialized host instance valid for the returned borrow's lifetime.
pub type MappedStringGetFn = unsafe extern "C" fn(ptr: *const c_void) -> StrView;

/// Callback updating an opaque host string component/resource at `ptr` from a UTF-8 [`StrView`].
///
/// # Safety
/// - `ptr` must point to a valid exclusively borrowed host instance.
/// - `view` must reference valid initialized bytes when `view.len > 0`.
pub type MappedStringSetFn = unsafe extern "C" fn(ptr: *mut c_void, view: StrView) -> FfiResult;

/// Generic `#[repr(C)]` VTable bridging an opaque UTF-8 string host component or resource
/// (such as `bevy_ecs::name::Name`) via [`StrView`].
#[repr(C)]
#[derive(Copy, Clone)]
pub struct MappedStringVTable {
    /// Leading [`VTableHeader`] at byte offset `0`.
    pub header: VTableHeader,
    /// Borrows the current UTF-8 string slice.
    pub get: Option<MappedStringGetFn>,
    /// Replaces the UTF-8 string value in place (`None` when read-only).
    pub set: Option<MappedStringSetFn>,
}

/// Wire classification tag for a [`BamFieldDesc`] entry (`typedef uint32_t BamFieldKind;`).
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct BamFieldKind(pub u32);

impl BamFieldKind {
    /// Unclassified or unknown field type (`0`).
    pub const UNKNOWN: Self = Self(0);
    /// `bool` / `FfiBool` (`1`).
    pub const BOOL: Self = Self(1);
    /// Unsigned 8-bit integer `u8` (`2`).
    pub const U8: Self = Self(2);
    /// Signed 8-bit integer `i8` (`3`).
    pub const I8: Self = Self(3);
    /// Unsigned 16-bit integer `u16` (`4`).
    pub const U16: Self = Self(4);
    /// Signed 16-bit integer `i16` (`5`).
    pub const I16: Self = Self(5);
    /// Unsigned 32-bit integer `u32` (`6`).
    pub const U32: Self = Self(6);
    /// Signed 32-bit integer `i32` (`7`).
    pub const I32: Self = Self(7);
    /// Unsigned 64-bit integer `u64` (`8`).
    pub const U64: Self = Self(8);
    /// Signed 64-bit integer `i64` (`9`).
    pub const I64: Self = Self(9);
    /// 32-bit IEEE 754 float `f32` (`10`).
    pub const F32: Self = Self(10);
    /// 64-bit IEEE 754 float `f64` (`11`).
    pub const F64: Self = Self(11);
    /// Platform pointer-sized unsigned integer `usize` (`12`).
    pub const USIZE: Self = Self(12);
    /// Platform pointer-sized signed integer `isize` (`13`).
    pub const ISIZE: Self = Self(13);
    /// Host-owned UTF-8 `String` exposed via `StrView` (`14`).
    pub const STRING: Self = Self(14);
    /// Host-owned typed `Vec<T>` exposed via `FfiSlice<T>` (`15`).
    pub const VEC: Self = Self(15);
    /// Cross-language `FfiOption<T>` (`16`).
    pub const FFI_OPTION: Self = Self(16);
    /// Nested `#[repr(C)]` struct or array (`17`).
    pub const NESTED: Self = Self(17);
    /// Opaque / skipped / non-inspectable field (`18`).
    pub const OPAQUE: Self = Self(18);
}

/// Static compile-time field reflection metadata generated by `#[derive(ModComponent)]`
/// (`typedef struct BamFieldDesc BamFieldDesc;`).
///
/// Layout:
/// - 64-bit: `64` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `36` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamFieldDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Wire classification tag [`BamFieldKind`] (`offset 4`).
    pub kind: BamFieldKind,
    /// UTF-8 field identifier (`offset 8`).
    pub name: StrView,
    /// Normalized Rust/ABI type tag (`offset 24` / `16`).
    pub type_tag: StrView,
    /// Byte offset inside the outer component (`offset 40` / `24`).
    pub offset: usize,
    /// Field byte size (`size_of::<Field>()`, `offset 48` / `28`).
    pub size: usize,
    /// Field byte alignment (`align_of::<Field>()`, `offset 56` / `32`).
    pub align: usize,
}

impl BamFieldDesc {
    /// Minimum size of `BamFieldDesc` in ABI `0.3` (`64` on 64-bit, `36` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a `BamFieldDesc` stamped with `struct_size = size_of::<Self>()`.
    #[inline]
    #[must_use]
    pub const fn new(
        kind: BamFieldKind,
        name: StrView,
        type_tag: StrView,
        offset: usize,
        size: usize,
        align: usize,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            kind,
            name,
            type_tag,
            offset,
            size,
            align,
        }
    }
}

// SAFETY: `BamFieldDesc` stores `'static` field names/tags and constant layout integers.
unsafe impl Send for BamFieldDesc {}
// SAFETY: See `Send` implementation above; enables `static BAM_FIELDS: &[BamFieldDesc]`.
unsafe impl Sync for BamFieldDesc {}

const _: () = {
    #[cfg(target_pointer_width = "64")]
    {
        assert!(core::mem::size_of::<BamFieldDesc>() == 64);
        assert!(core::mem::align_of::<BamFieldDesc>() == 8);
        assert!(core::mem::offset_of!(BamFieldDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamFieldDesc, kind) == 4);
        assert!(core::mem::offset_of!(BamFieldDesc, name) == 8);
        assert!(core::mem::offset_of!(BamFieldDesc, type_tag) == 24);
        assert!(core::mem::offset_of!(BamFieldDesc, offset) == 40);
        assert!(core::mem::offset_of!(BamFieldDesc, size) == 48);
        assert!(core::mem::offset_of!(BamFieldDesc, align) == 56);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(core::mem::size_of::<BamFieldDesc>() == 36);
        assert!(core::mem::align_of::<BamFieldDesc>() == 4);
        assert!(core::mem::offset_of!(BamFieldDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamFieldDesc, kind) == 4);
        assert!(core::mem::offset_of!(BamFieldDesc, name) == 8);
        assert!(core::mem::offset_of!(BamFieldDesc, type_tag) == 16);
        assert!(core::mem::offset_of!(BamFieldDesc, offset) == 24);
        assert!(core::mem::offset_of!(BamFieldDesc, size) == 28);
        assert!(core::mem::offset_of!(BamFieldDesc, align) == 32);
    }
};

/// Custom initializer callback invoked by the host when materializing a required component
/// (`typedef BamResult (*BamRequiredInitFn)(void* dst, void* user_data);`).
///
/// # Safety
/// - `dst` must be non-null and aligned for the target component's `(size, align)`.
/// - `user_data` is the opaque pointer recorded in [`BamRequiredComponent::user_data`].
/// - The callback must initialize all `size` bytes of `dst` on `OK` and never unwind across FFI.
pub type BamRequiredInitFn =
    unsafe extern "C" fn(dst: *mut c_void, user_data: *mut c_void) -> FfiResult;

/// Required component descriptor supplies static POD custom bytes via [`BamRequiredComponent::init_bytes`].
pub const BAM_REQUIRED_FLAG_CUSTOM_BYTES: u32 = 1 << 0;

/// Required component descriptor supplies custom initialization callback [`BamRequiredComponent::init_fn`].
pub const BAM_REQUIRED_FLAG_INIT_FN: u32 = 1 << 1;

/// Required component descriptor identifies its target by [`BamRequiredComponent::uuid`] instead
/// of a pre-resolved [`BamRequiredComponent::component`] handle.
pub const BAM_REQUIRED_FLAG_RESOLVE_BY_UUID: u32 = 1 << 2;

/// Bitmask of all recognized [`BamRequiredComponent::flags`] bits in ABI `0.3`.
pub const BAM_REQUIRED_KNOWN_FLAGS: u32 =
    BAM_REQUIRED_FLAG_CUSTOM_BYTES | BAM_REQUIRED_FLAG_INIT_FN | BAM_REQUIRED_FLAG_RESOLVE_BY_UUID;

/// Descriptor for a required component relationship (`#[mod_component(require(Comp = expr))]`)
/// (`typedef struct BamRequiredComponent BamRequiredComponent;`).
///
/// Layout:
/// - 64-bit: `64` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `48` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamRequiredComponent {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Bitmask of `BAM_REQUIRED_FLAG_*` (`offset 4`).
    pub flags: u32,
    /// Resolved target component handle (`BamComponent::INVALID` when `RESOLVE_BY_UUID` is set, `offset 8`).
    pub component: BamComponent,
    /// Explicit zero-reserved field (`must be 0`, `offset 12`).
    pub reserved0: u32,
    /// Deterministic 128-bit UUID of the required component (`offset 16`).
    pub uuid: ComponentUuid,
    /// Optional custom POD initialization byte slice pointer (`offset 32`).
    pub init_bytes: *const u8,
    /// Target byte length (`init_bytes` length or output size expected by `init_fn`, `offset 40` / `36`).
    pub init_size: usize,
    /// Optional custom initializer callback invoked when `BAM_REQUIRED_FLAG_INIT_FN` is set (`offset 48` / `40`).
    pub init_fn: Option<BamRequiredInitFn>,
    /// Opaque user context passed to `init_fn` (`offset 56` / `44`).
    pub user_data: *mut c_void,
}

impl BamRequiredComponent {
    /// Minimum size of `BamRequiredComponent` in ABI `0.3` (`64` on 64-bit, `48` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a required component entry resolving by pre-registered [`BamComponent`] handle
    /// using the component's default constructor.
    #[inline]
    #[must_use]
    pub const fn by_handle(component: BamComponent) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            component,
            reserved0: 0,
            uuid: ComponentUuid::ZERO,
            init_bytes: core::ptr::null(),
            init_size: 0,
            init_fn: None,
            user_data: core::ptr::null_mut(),
        }
    }

    /// Constructs a required component entry resolving by [`ComponentUuid`] (for static
    /// [`ModComponent::REQUIRED_COMPONENTS`](crate::ModComponent::REQUIRED_COMPONENTS) tables).
    #[inline]
    #[must_use]
    pub const fn by_uuid(uuid: ComponentUuid) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: BAM_REQUIRED_FLAG_RESOLVE_BY_UUID,
            component: BamComponent::INVALID,
            reserved0: 0,
            uuid,
            init_bytes: core::ptr::null(),
            init_size: 0,
            init_fn: None,
            user_data: core::ptr::null_mut(),
        }
    }

    /// Attaches a custom initialization callback and expected target byte size.
    #[inline]
    #[must_use]
    pub const fn with_init_fn(
        mut self,
        init_fn: BamRequiredInitFn,
        init_size: usize,
        user_data: *mut c_void,
    ) -> Self {
        self.flags |= BAM_REQUIRED_FLAG_INIT_FN;
        self.init_fn = Some(init_fn);
        self.init_size = init_size;
        self.user_data = user_data;
        self
    }

    /// Attaches static POD custom initialization bytes.
    #[inline]
    #[must_use]
    pub const fn with_custom_bytes(mut self, bytes: &'static [u8]) -> Self {
        self.flags |= BAM_REQUIRED_FLAG_CUSTOM_BYTES;
        self.init_bytes = bytes.as_ptr();
        self.init_size = bytes.len();
        self
    }
}

// SAFETY: `BamRequiredComponent` holds immutable `'static` metadata and optional `extern "C"` fn pointers.
unsafe impl Send for BamRequiredComponent {}
// SAFETY: See `Send` implementation above; enables `static REQUIRED_COMPONENTS: &[BamRequiredComponent]`.
unsafe impl Sync for BamRequiredComponent {}

const _: () = {
    #[cfg(target_pointer_width = "64")]
    {
        assert!(core::mem::size_of::<BamRequiredComponent>() == 64);
        assert!(core::mem::align_of::<BamRequiredComponent>() == 8);
        assert!(core::mem::offset_of!(BamRequiredComponent, struct_size) == 0);
        assert!(core::mem::offset_of!(BamRequiredComponent, flags) == 4);
        assert!(core::mem::offset_of!(BamRequiredComponent, component) == 8);
        assert!(core::mem::offset_of!(BamRequiredComponent, reserved0) == 12);
        assert!(core::mem::offset_of!(BamRequiredComponent, uuid) == 16);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_bytes) == 32);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_size) == 40);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_fn) == 48);
        assert!(core::mem::offset_of!(BamRequiredComponent, user_data) == 56);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(core::mem::size_of::<BamRequiredComponent>() == 48);
        assert!(core::mem::align_of::<BamRequiredComponent>() == 4);
        assert!(core::mem::offset_of!(BamRequiredComponent, struct_size) == 0);
        assert!(core::mem::offset_of!(BamRequiredComponent, flags) == 4);
        assert!(core::mem::offset_of!(BamRequiredComponent, component) == 8);
        assert!(core::mem::offset_of!(BamRequiredComponent, reserved0) == 12);
        assert!(core::mem::offset_of!(BamRequiredComponent, uuid) == 16);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_bytes) == 32);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_size) == 36);
        assert!(core::mem::offset_of!(BamRequiredComponent, init_fn) == 40);
        assert!(core::mem::offset_of!(BamRequiredComponent, user_data) == 44);
    }
};

/// Exclusive relationship flag: attaching a new target replaces any prior edge of this relation.
pub const BAM_RELATION_FLAG_EXCLUSIVE: u32 = 1 << 0;

/// Cascade-despawn relationship flag: despawning the target also despawns dependent source entities.
pub const BAM_RELATION_FLAG_CASCADE_DESPAWN: u32 = 1 << 1;

/// Symmetric relationship flag: edge `(A -> B)` implies `(B -> A)`.
pub const BAM_RELATION_FLAG_SYMMETRIC: u32 = 1 << 2;

/// Bitmask of all recognized [`BamRelationDesc::flags`] bits in ABI `0.3`.
pub const BAM_RELATION_KNOWN_FLAGS: u32 =
    BAM_RELATION_FLAG_EXCLUSIVE | BAM_RELATION_FLAG_CASCADE_DESPAWN | BAM_RELATION_FLAG_SYMMETRIC;

/// Descriptor registering a typed entity relationship edge (`typedef struct BamRelationDesc BamRelationDesc;`).
///
/// Layout:
/// - 64-bit: `56` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `44` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamRelationDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Bitmask of `BAM_RELATION_FLAG_*` (`offset 4`).
    pub flags: u32,
    /// Canonical NUL-terminated UTF-8 relation name (`offset 8`).
    pub name: *const c_char,
    /// Deterministic 128-bit UUID identifying the relation (`offset 16` / `12`).
    pub uuid: ComponentUuid,
    /// Edge payload byte size (`0` for tag/marker relations, `offset 32` / `28`).
    pub data_size: usize,
    /// Edge payload byte alignment (`1` when `data_size == 0`, `offset 40` / `32`).
    pub data_align: usize,
    /// Optional out-degree cap (`0` = unlimited, `1` for exclusive if flagged, `offset 48` / `36`).
    pub max_degree: u32,
    /// Explicit zero-reserved field (`must be 0`, `offset 52` / `40`).
    pub reserved0: u32,
}

impl BamRelationDesc {
    /// Minimum size of `BamRelationDesc` in ABI `0.3` (`56` on 64-bit, `44` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a marker (zero-sized payload) relationship descriptor.
    #[inline]
    pub const fn new(name: *const c_char, uuid: ComponentUuid) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            name,
            uuid,
            data_size: 0,
            data_align: 1,
            max_degree: 0,
            reserved0: 0,
        }
    }

    /// Sets `BAM_RELATION_FLAG_*` bitflags on this descriptor.
    #[inline]
    pub const fn with_flags(mut self, flags: u32) -> Self {
        self.flags = flags;
        self
    }

    /// Attaches an edge payload `(data_size, data_align)`.
    #[inline]
    pub const fn with_payload_layout(mut self, data_size: usize, data_align: usize) -> Self {
        self.data_size = data_size;
        self.data_align = data_align;
        self
    }

    /// Validates `struct_size`, known `flags`, zero `reserved0`, non-null `name`, and payload layout.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_3_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_RELATION_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.name.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if !self.data_align.is_power_of_two() || !self.data_size.is_multiple_of(self.data_align) {
            return FfiResult::INVALID_ARGUMENT;
        }
        FfiResult::OK
    }
}

/// Output descriptor returned by `component_layout_info` (`typedef struct BamLayoutInfo BamLayoutInfo;`).
///
/// Layout:
/// - 64-bit: `64` bytes, `8`-byte alignment (`schema_fingerprint` at offset `24`), zero padding.
/// - 32-bit: `48` bytes, `align = align_of::<u64>()` (`schema_fingerprint` at offset `24`), zero padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamLayoutInfo {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Host storage/metadata flags (`BAM_STORAGE_FLAG_*`, `offset 4`).
    pub flags: u32,
    /// Deterministic 128-bit component UUID (`offset 8`).
    pub uuid: ComponentUuid,
    /// Deterministic 64-bit FNV-1a schema fingerprint (`offset 24`).
    pub schema_fingerprint: u64,
    /// Component byte size (`size_of::<T>()`, `offset 32`).
    pub size: usize,
    /// Component byte alignment (`align_of::<T>()`, `offset 40` / `36`).
    pub align: usize,
    /// Pointer to immutable host-retained [`BamFieldDesc`] slice (`offset 48` / `40`).
    pub fields: *const BamFieldDesc,
    /// Number of [`BamFieldDesc`] entries (`offset 56` / `44`).
    pub field_count: usize,
}

impl BamLayoutInfo {
    /// Minimum size of `BamLayoutInfo` in ABI `0.3` (`64` on 64-bit, `48` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a zeroed `BamLayoutInfo` buffer stamped with `struct_size`.
    #[inline]
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            uuid: ComponentUuid::ZERO,
            schema_fingerprint: 0,
            size: 0,
            align: 1,
            fields: core::ptr::null(),
            field_count: 0,
        }
    }
}

/// Row migration callback invoked per live entity during `migrate_component_column`
/// (`typedef BamResult (*BamRowMigrateFn)(const void* src_row, void* dst_row, void* user_data);`).
///
/// # Safety
/// - `src_row` points to a readable, initialized component instance under the old layout.
/// - `dst_row` points to uninitialized, properly aligned writable storage under `new_desc`.
/// - The callback must never unwind across FFI.
pub type BamRowMigrateFn = unsafe extern "C" fn(
    src_row: *const c_void,
    dst_row: *mut c_void,
    user_data: *mut c_void,
) -> FfiResult;

/// Bitmask of recognized [`BamScheduleSetDesc::flags`] bits in ABI `0.3` (`0`).
pub const BAM_SCHEDULE_SET_KNOWN_FLAGS: u32 = 0;

/// Descriptor configuring a named schedule system set and ordering edges
/// (`typedef struct BamScheduleSetDesc BamScheduleSetDesc;`).
///
/// Layout:
/// - 64-bit: `48` bytes, `8`-byte alignment, zero implicit padding.
/// - 32-bit: `36` bytes, `4`-byte alignment, zero implicit padding.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct BamScheduleSetDesc {
    /// Size of this struct in bytes (`offset 0`).
    pub struct_size: u32,
    /// Reserved flags (`0` in ABI `0.3`, `offset 4`).
    pub flags: u32,
    /// Target schedule (`ScheduleId`, `offset 8`).
    pub schedule: ScheduleId,
    /// Number of handles in `before_sets` (`offset 12`).
    pub before_count: u32,
    /// Number of handles in `after_sets` (`offset 16`).
    pub after_count: u32,
    /// Explicit zero-reserved field (`must be 0`, `offset 20`).
    pub reserved0: u32,
    /// NUL-terminated UTF-8 set name (`offset 24`).
    pub name: *const c_char,
    /// Optional slice of sets ordered after this set (`offset 32` / `28`).
    pub before_sets: *const crate::primitives::BamSystemSet,
    /// Optional slice of sets ordered before this set (`offset 40` / `32`).
    pub after_sets: *const crate::primitives::BamSystemSet,
}

impl BamScheduleSetDesc {
    /// Minimum size of `BamScheduleSetDesc` in ABI `0.3` (`48` on 64-bit, `36` on 32-bit).
    pub const MIN_V0_3_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a `BamScheduleSetDesc` with empty ordering edges in `ScheduleId::UPDATE`.
    #[inline]
    pub const fn new(name: *const c_char) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            flags: 0,
            schedule: ScheduleId::UPDATE,
            before_count: 0,
            after_count: 0,
            reserved0: 0,
            name,
            before_sets: core::ptr::null(),
            after_sets: core::ptr::null(),
        }
    }

    /// Validates `struct_size`, known flags, `reserved0 == 0`, `schedule`, non-null `name`,
    /// and edge pointer non-nullness.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V0_3_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if (self.flags & !BAM_SCHEDULE_SET_KNOWN_FLAGS) != 0 {
            return FfiResult::UNSUPPORTED;
        }
        if self.reserved0 != 0 || !self.schedule.is_valid() {
            return FfiResult::INVALID_ARGUMENT;
        }
        if self.name.is_null() {
            return FfiResult::NULL_POINTER;
        }
        if (self.before_count > 0 && self.before_sets.is_null())
            || (self.after_count > 0 && self.after_sets.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

const _: () = {
    #[cfg(target_pointer_width = "64")]
    {
        assert!(core::mem::size_of::<BamRelationDesc>() == 56);
        assert!(core::mem::align_of::<BamRelationDesc>() == 8);
        assert!(core::mem::offset_of!(BamRelationDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamRelationDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamRelationDesc, name) == 8);
        assert!(core::mem::offset_of!(BamRelationDesc, uuid) == 16);
        assert!(core::mem::offset_of!(BamRelationDesc, data_size) == 32);
        assert!(core::mem::offset_of!(BamRelationDesc, data_align) == 40);
        assert!(core::mem::offset_of!(BamRelationDesc, max_degree) == 48);
        assert!(core::mem::offset_of!(BamRelationDesc, reserved0) == 52);

        assert!(core::mem::size_of::<BamLayoutInfo>() == 64);
        assert!(core::mem::align_of::<BamLayoutInfo>() == 8);
        assert!(core::mem::offset_of!(BamLayoutInfo, struct_size) == 0);
        assert!(core::mem::offset_of!(BamLayoutInfo, flags) == 4);
        assert!(core::mem::offset_of!(BamLayoutInfo, uuid) == 8);
        assert!(core::mem::offset_of!(BamLayoutInfo, schema_fingerprint) == 24);
        assert!(core::mem::offset_of!(BamLayoutInfo, size) == 32);
        assert!(core::mem::offset_of!(BamLayoutInfo, align) == 40);
        assert!(core::mem::offset_of!(BamLayoutInfo, fields) == 48);
        assert!(core::mem::offset_of!(BamLayoutInfo, field_count) == 56);

        assert!(core::mem::size_of::<BamScheduleSetDesc>() == 48);
        assert!(core::mem::align_of::<BamScheduleSetDesc>() == 8);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, schedule) == 8);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, before_count) == 12);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, after_count) == 16);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, reserved0) == 20);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, name) == 24);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, before_sets) == 32);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, after_sets) == 40);
    }
    #[cfg(target_pointer_width = "32")]
    {
        assert!(core::mem::size_of::<BamRelationDesc>() == 44);
        assert!(core::mem::align_of::<BamRelationDesc>() == 4);
        assert!(core::mem::offset_of!(BamRelationDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamRelationDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamRelationDesc, name) == 8);
        assert!(core::mem::offset_of!(BamRelationDesc, uuid) == 12);
        assert!(core::mem::offset_of!(BamRelationDesc, data_size) == 28);
        assert!(core::mem::offset_of!(BamRelationDesc, data_align) == 32);
        assert!(core::mem::offset_of!(BamRelationDesc, max_degree) == 36);
        assert!(core::mem::offset_of!(BamRelationDesc, reserved0) == 40);

        assert!(core::mem::size_of::<BamLayoutInfo>() == 48);
        assert!(core::mem::align_of::<BamLayoutInfo>() == core::mem::align_of::<u64>());
        assert!(core::mem::offset_of!(BamLayoutInfo, struct_size) == 0);
        assert!(core::mem::offset_of!(BamLayoutInfo, flags) == 4);
        assert!(core::mem::offset_of!(BamLayoutInfo, uuid) == 8);
        assert!(core::mem::offset_of!(BamLayoutInfo, schema_fingerprint) == 24);
        assert!(core::mem::offset_of!(BamLayoutInfo, size) == 32);
        assert!(core::mem::offset_of!(BamLayoutInfo, align) == 36);
        assert!(core::mem::offset_of!(BamLayoutInfo, fields) == 40);
        assert!(core::mem::offset_of!(BamLayoutInfo, field_count) == 44);

        assert!(core::mem::size_of::<BamScheduleSetDesc>() == 36);
        assert!(core::mem::align_of::<BamScheduleSetDesc>() == 4);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, struct_size) == 0);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, flags) == 4);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, schedule) == 8);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, before_count) == 12);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, after_count) == 16);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, reserved0) == 20);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, name) == 24);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, before_sets) == 28);
        assert!(core::mem::offset_of!(BamScheduleSetDesc, after_sets) == 32);
    }
};

// SAFETY: `BamFieldDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamFieldDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_3_SIZE;
}

// SAFETY: `BamRequiredComponent` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamRequiredComponent {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_3_SIZE;
}

// SAFETY: `BamRelationDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamRelationDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_3_SIZE;
}

// SAFETY: `BamLayoutInfo` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamLayoutInfo {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_3_SIZE;
}

// SAFETY: `BamScheduleSetDesc` is `#[repr(C)]`, starts with `struct_size: u32` at offset 0, and all
// fields are valid when zero-filled.
unsafe impl ExtensibleStruct for BamScheduleSetDesc {
    const MIN_V0_2_SIZE: usize = Self::MIN_V0_3_SIZE;
}
