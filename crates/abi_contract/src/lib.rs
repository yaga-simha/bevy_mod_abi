#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `abi_contract`
//!
//! Zero-dependency `#![no_std]` C-ABI contract definitions for `bevy_abi_mod`.
//! All types are `#[repr(C)]` or explicit primitive representations to guarantee
//! zero shared Rust ABI assumptions across dynamic library boundaries.

use core::ffi::{c_char, c_void};

pub const ABI_VERSION_MAJOR: u32 = 0;
pub const ABI_VERSION_MINOR: u32 = 1;
pub const ABI_VERSION_PATCH: u32 = 0;

/// FFI result status codes returned across the C-ABI boundary.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum FfiResult {
    Ok = 0,
    InvalidComponent = 1,
    AccessConflict = 2,
    PanicCaught = 3,
    NullPointer = 4,
    InvalidUtf8 = 5,
    PermissionDenied = 6,
    InvalidVersion = 7,
    InternalError = 99,
}

/// Legacy/Signed status return codes for C-ABI lifecycle calls.
#[repr(i32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum ModStatus {
    Success = 0,
    ErrInvalidVersion = -1,
    ErrNullPointer = -2,
    ErrComponentNotFound = -3,
    ErrAccessConflict = -4,
    ErrPanicCaught = -5,
    ErrPermissionDenied = -6,
    ErrInternal = -99,
}

impl From<FfiResult> for ModStatus {
    fn from(res: FfiResult) -> Self {
        match res {
            FfiResult::Ok => ModStatus::Success,
            FfiResult::InvalidComponent => ModStatus::ErrComponentNotFound,
            FfiResult::AccessConflict => ModStatus::ErrAccessConflict,
            FfiResult::PanicCaught => ModStatus::ErrPanicCaught,
            FfiResult::NullPointer => ModStatus::ErrNullPointer,
            FfiResult::InvalidUtf8 => ModStatus::ErrInternal,
            FfiResult::PermissionDenied => ModStatus::ErrPermissionDenied,
            FfiResult::InvalidVersion => ModStatus::ErrInvalidVersion,
            FfiResult::InternalError => ModStatus::ErrInternal,
        }
    }
}

/// Packed 64-bit entity representation matching `bevy_ecs::entity::Entity::to_bits()`.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiEntity(pub u64);

impl FfiEntity {
    pub const PLACEHOLDER: Self = Self(u64::MAX);

    #[inline]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn index(self) -> u32 {
        self.0 as u32
    }

    #[inline]
    pub const fn generation(self) -> u32 {
        (self.0 >> 32) as u32
    }

    #[inline]
    pub const fn from_parts(index: u32, generation: u32) -> Self {
        Self((index as u64) | ((generation as u64) << 32))
    }
}

/// Explicit index + generation entity identifier for C99 convenience.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct EntityId {
    pub index: u32,
    pub generation: u32,
}

impl From<FfiEntity> for EntityId {
    #[inline]
    fn from(entity: FfiEntity) -> Self {
        Self {
            index: entity.index(),
            generation: entity.generation(),
        }
    }
}

impl From<EntityId> for FfiEntity {
    #[inline]
    fn from(id: EntityId) -> Self {
        Self::from_parts(id.index, id.generation)
    }
}

/// ABI-safe non-owning slice representation (`#[repr(C)]` replacement for `&[T]`).
#[repr(C)]
#[derive(Debug)]
pub struct FfiSlice<T> {
    pub ptr: *const T,
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

/// Opaque string view across the C ABI boundary.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct StrView {
    pub ptr: *const u8,
    pub len: usize,
}

impl StrView {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            ptr: core::ptr::null(),
            len: 0,
        }
    }

    #[inline]
    pub const fn from_str(s: &str) -> Self {
        Self {
            ptr: s.as_ptr(),
            len: s.len(),
        }
    }

    /// Convert `StrView` to a UTF-8 `&str`.
    ///
    /// # Safety
    /// If `self.len > 0`, `self.ptr` must be valid for `self.len` bytes for lifetime `'a`.
    #[inline]
    pub unsafe fn as_str<'a>(&self) -> Result<&'a str, core::str::Utf8Error> {
        if self.len == 0 || self.ptr.is_null() {
            Ok("")
        } else {
            // SAFETY: Caller guarantees `self.ptr` points to `self.len` valid bytes.
            let bytes = unsafe { core::slice::from_raw_parts(self.ptr, self.len) };
            core::str::from_utf8(bytes)
        }
    }
}

impl From<FfiSlice<u8>> for StrView {
    #[inline]
    fn from(slice: FfiSlice<u8>) -> Self {
        Self {
            ptr: slice.ptr,
            len: slice.len,
        }
    }
}

impl From<StrView> for FfiSlice<u8> {
    #[inline]
    fn from(view: StrView) -> Self {
        Self {
            ptr: view.ptr,
            len: view.len,
        }
    }
}

/// Unique 128-bit component identifier (UUID-compatible).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ComponentUuid {
    pub bytes: [u8; 16],
}

impl ComponentUuid {
    /// Deterministic FNV-1a 128-bit hash from a component name string.
    pub const fn from_name(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut h1: u64 = 0xcbf29ce484222325;
        let mut h2: u64 = 0x84222325cbf29ce4;
        let mut i = 0;
        while i < bytes.len() {
            h1 ^= bytes[i] as u64;
            h1 = h1.wrapping_mul(0x100000001b3);
            h2 ^= (bytes[i] as u64).wrapping_add(i as u64);
            h2 = h2.wrapping_mul(0x100000001b3);
            i += 1;
        }
        let b1 = h1.to_le_bytes();
        let b2 = h2.to_le_bytes();
        Self {
            bytes: [
                b1[0], b1[1], b1[2], b1[3], b1[4], b1[5], b1[6], b1[7], b2[0], b2[1], b2[2], b2[3],
                b2[4], b2[5], b2[6], b2[7],
            ],
        }
    }
}

/// Access mode declaration for an ECS system.
#[repr(u32)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum ComponentAccessMode {
    Read = 0,
    Write = 1,
    FilterOnly = 2,
}

/// Component access descriptor using a null-terminated C string component name.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessDesc {
    pub name: *const c_char,
    pub is_mutable: bool,
    pub is_filter_only: bool,
}

/// Single component access requirement by UUID.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessRequirement {
    pub component_uuid: ComponentUuid,
    pub mode: ComponentAccessMode,
}

/// System access descriptor passed during system registration.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SystemAccessDescriptor {
    pub name: StrView,
    pub requirements: *const ComponentAccessRequirement,
    pub requirements_count: usize,
}

/// Opaque C handle representing an active Archetype Table chunk iterator.
#[repr(C)]
pub struct RawChunkIter {
    _private: [u8; 0],
}

/// Opaque C handle representing a deferred command queue passed to dynamic systems.
#[repr(C)]
pub struct RawCommandQueue {
    _private: [u8; 0],
}

/// Advance the chunk iterator to the next matching archetype table.
/// Returns `0` when exhausted, or the `entity_count` for the active chunk.
pub type IterNextChunkFn = unsafe extern "C-unwind" fn(iter: *mut RawChunkIter) -> usize;

/// Obtain a read-only byte pointer to the start of column `index` in the current chunk.
/// Returns `null` if `index` is out of bounds or is a filter-only component.
pub type IterColumnReadonlyFn =
    unsafe extern "C-unwind" fn(iter: *const RawChunkIter, index: usize) -> *const u8;

/// Obtain a mutable byte pointer to the start of column `index` in the current chunk.
/// Marks `ComponentTicks` as changed for all entities in the chunk.
/// Returns `null` if the column was not declared as mutable (`is_mutable: true`).
pub type IterColumnMutableFn =
    unsafe extern "C-unwind" fn(iter: *mut RawChunkIter, index: usize) -> *mut u8;

/// Mark a specific entity row's component tick as changed without marking the entire column,
/// or explicitly mark row `row` in column `index` as changed.
pub type IterMarkChangedFn =
    unsafe extern "C-unwind" fn(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult;

/// Obtain a pointer to the contiguous `FfiEntity` array for the current chunk.
pub type IterEntitiesFn =
    unsafe extern "C-unwind" fn(iter: *const RawChunkIter) -> *const FfiEntity;

/// Return the byte stride (`size_of::<C>()`) of column `index`.
pub type IterColumnStrideFn =
    unsafe extern "C-unwind" fn(iter: *const RawChunkIter, index: usize) -> usize;

/// Dynamic system callback invoked by the host during schedule execution.
pub type DynamicSystemCallback =
    unsafe extern "C-unwind" fn(iter: *mut RawChunkIter, commands: *mut RawCommandQueue);

/// Descriptor used by mods to register a `DynamicCSystem` with the host.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SystemDescriptor {
    pub name: *const c_char,
    pub queries: *const ComponentAccessDesc,
    pub query_count: usize,
    pub callback: DynamicSystemCallback,
}

/// Flecs-style contiguous table chunk iterator struct.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct EcsChunkIter {
    /// Number of matching entities in this contiguous chunk.
    pub count: usize,
    /// Pointer to contiguous array of EntityId (length = count).
    pub entities: *const EntityId,
    /// Array of column base pointers for requested components.
    pub column_ptrs: *const *mut c_void,
    /// Byte strides between successive elements in each column.
    pub column_strides: *const usize,
    /// Number of columns requested.
    pub column_count: usize,
}

/// System execution callback type provided by guest mods (legacy/direct chunk callback).
pub type SystemRunCallback = extern "C" fn(user_data: *mut c_void, chunk: *mut EcsChunkIter);

/// System registration function signature.
pub type RegisterSystemFn = extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const SystemAccessDescriptor,
    callback: SystemRunCallback,
    user_data: *mut c_void,
) -> ModStatus;

/// Symbol-resolver function pointer passed to the guest mod entry point (Godot GDExtension style).
pub type GetProcAddressFn = unsafe extern "C-unwind" fn(name: *const c_char) -> *const c_void;

/// Host API VTable populated or queried at handshake time.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HostApi {
    pub register_system: RegisterSystemFn,
}

/// Mod initialization entry point function signature.
pub type ModInitFn = extern "C" fn(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> ModStatus;

/// Mod teardown function signature.
pub type ModShutdownFn = extern "C" fn(host_context: *mut c_void) -> ModStatus;

/// Entry mapping an exported C symbol name to its function or static VTable pointer.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SymbolEntry {
    pub name: &'static str,
    pub ptr: *const c_void,
}

// SAFETY: `SymbolEntry` only holds a `'static` string reference and a static function/VTable pointer.
unsafe impl Sync for SymbolEntry {}
// SAFETY: `SymbolEntry` only holds immutable static pointers safe to share across threads.
unsafe impl Send for SymbolEntry {}

/// Trait implemented by `#[derive(ModExposable)]` on host components.
///
/// Exposes the component's string name, deterministic UUID, layout (`size` and `align`),
/// static `#[repr(C)]` VTable, and C-ABI accessor symbol table for `ModRegistry` registration.
pub trait ExposableComponent: Sized + 'static {
    /// The string name of the component (e.g., `"Position"`).
    const COMPONENT_NAME: &'static str;

    /// Null-terminated C string bytes of the component name (e.g., `b"Position\0"`).
    const COMPONENT_CSTR: &'static [u8];

    /// Deterministic 128-bit UUID for this component.
    const UUID: ComponentUuid;

    /// The generated `#[repr(C)]` VTable struct type (`<Struct>VTable`).
    type VTable: Copy + 'static;

    /// Static constant value of the component's VTable.
    const VTABLE: Self::VTable;

    /// Returns a pointer to the static VTable instance for this component.
    fn vtable_ptr() -> *const Self::VTable;

    /// Returns a type-erased pointer to the static VTable instance.
    #[inline]
    fn erased_vtable_ptr() -> *const c_void {
        Self::vtable_ptr() as *const c_void
    }

    /// Exact byte stride (`core::mem::size_of::<Self>()`) for contiguous array iteration.
    #[inline]
    fn size() -> usize {
        core::mem::size_of::<Self>()
    }

    /// Exact byte alignment (`core::mem::align_of::<Self>()`).
    #[inline]
    fn align() -> usize {
        core::mem::align_of::<Self>()
    }

    /// Returns all exported C-ABI accessor symbols and VTable pointers for this component.
    fn symbols() -> &'static [SymbolEntry];

    /// Look up a symbol pointer by name (e.g. `"Position_get_x"`, `"Position_vtable"`).
    #[inline]
    fn resolve_symbol(name: &str) -> Option<*const c_void> {
        let syms = Self::symbols();
        let mut i = 0;
        while i < syms.len() {
            if syms[i].name.as_bytes() == name.as_bytes() {
                return Some(syms[i].ptr);
            }
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ffi_entity_roundtrip() {
        let entity = FfiEntity::from_parts(42, 7);
        assert_eq!(entity.index(), 42);
        assert_eq!(entity.generation(), 7);

        let id: EntityId = entity.into();
        assert_eq!(id.index, 42);
        assert_eq!(id.generation, 7);

        let roundtrip: FfiEntity = id.into();
        assert_eq!(roundtrip, entity);
    }

    #[test]
    fn test_ffi_slice_and_str_view() {
        let text = "hello_mod";
        let slice = FfiSlice::from_str(text);
        assert!(!slice.is_empty());
        assert_eq!(slice.len, 9);
        // SAFETY: `slice` was constructed from a valid static `&str`.
        let recovered = unsafe { slice.as_str().unwrap() };
        assert_eq!(recovered, "hello_mod");

        let view: StrView = slice.into();
        // SAFETY: `view` points to the same valid static `&str`.
        assert_eq!(unsafe { view.as_str().unwrap() }, "hello_mod");

        let empty = FfiSlice::<u8>::empty();
        assert!(empty.is_empty());
        // SAFETY: `empty` has length 0.
        assert_eq!(unsafe { empty.as_slice() }, &[] as &[u8]);
    }

    #[test]
    fn test_component_uuid_deterministic() {
        let u1 = ComponentUuid::from_name("Position");
        let u2 = ComponentUuid::from_name("Position");
        let u3 = ComponentUuid::from_name("Velocity");
        assert_eq!(u1, u2);
        assert_ne!(u1, u3);
    }
}
