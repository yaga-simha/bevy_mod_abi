#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `abi_contract`
//!
//! Zero-dependency `#![no_std]` C-ABI contract definitions for `bevy_abi_mod`.
//!
//! ## Soundness Guarantees
//! - Every struct crossing the FFI boundary is `#[repr(C)]` or `#[repr(transparent)]`.
//! - Status codes (`FfiResult`) and access modes (`ComponentAccessMode`) are `#[repr(transparent)]`
//!   `u32` newtypes rather than Rust `enum`s, preventing undefined behavior if foreign C/C++/Zig
//!   code returns or passes an out-of-range discriminant.
//! - Every function pointer crossing the boundary is `unsafe extern "C" fn`, ensuring
//!   foreign calls require an explicit `unsafe` block and preventing undefined unwinding across
//!   foreign or asymmetric Rust runtime stack frames.

use core::ffi::{c_char, c_void};

pub const ABI_VERSION_MAJOR: u32 = 0;
pub const ABI_VERSION_MINOR: u32 = 1;
pub const ABI_VERSION_PATCH: u32 = 0;

/// Packed 32-bit ABI version: `(MAJOR << 24) | (MINOR << 16) | PATCH`.
pub const ABI_VERSION_PACKED: u32 =
    (ABI_VERSION_MAJOR << 24) | (ABI_VERSION_MINOR << 16) | ABI_VERSION_PATCH;

/// FFI-safe status code returned across the C-ABI boundary.
///
/// Represented as a `#[repr(transparent)]` `u32` newtype rather than a Rust `enum`
/// so that arbitrary integer values returned by foreign C/C++ code can never trigger
/// invalid-discriminant undefined behavior in Rust.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiResult(pub u32);

#[allow(non_upper_case_globals)]
impl FfiResult {
    pub const OK: Self = Self(0);
    pub const INVALID_COMPONENT: Self = Self(1);
    pub const ACCESS_CONFLICT: Self = Self(2);
    pub const PANIC_CAUGHT: Self = Self(3);
    pub const NULL_POINTER: Self = Self(4);
    pub const INVALID_UTF8: Self = Self(5);
    pub const PERMISSION_DENIED: Self = Self(6);
    pub const INVALID_VERSION: Self = Self(7);
    pub const INTERNAL_ERROR: Self = Self(99);

    // PascalCase aliases for ergonomic pattern matching
    pub const Ok: Self = Self::OK;
    pub const InvalidComponent: Self = Self::INVALID_COMPONENT;
    pub const AccessConflict: Self = Self::ACCESS_CONFLICT;
    pub const PanicCaught: Self = Self::PANIC_CAUGHT;
    pub const NullPointer: Self = Self::NULL_POINTER;
    pub const InvalidUtf8: Self = Self::INVALID_UTF8;
    pub const PermissionDenied: Self = Self::PERMISSION_DENIED;
    pub const InvalidVersion: Self = Self::INVALID_VERSION;
    pub const InternalError: Self = Self::INTERNAL_ERROR;

    #[inline]
    pub const fn is_ok(self) -> bool {
        self.0 == Self::OK.0
    }

    #[inline]
    pub const fn is_err(self) -> bool {
        self.0 != Self::OK.0
    }
}

/// Opaque 64-bit entity identifier matching `bevy_ecs::entity::Entity::to_bits()`.
///
/// Do not assume a specific bit split between index and generation across Bevy versions;
/// always convert via `bevy_ecs::entity::Entity::try_from_bits` / `to_bits` on the host.
#[repr(transparent)]
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
}

/// ABI-safe non-owning slice representation (`#[repr(C)]` replacement for `&[T]`).
///
/// # Borrow Lifetime
/// When returned by a generated component getter (e.g., `<Struct>_get_<field>`),
/// the returned `FfiSlice` borrows directly from the host's component storage.
/// It remains valid only until the next mutation of that field, structural ECS change,
/// or the end of the current chunk callback.
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

/// Non-owning UTF-8 string view across the C ABI boundary (`FfiSlice<u8>` alias).
pub type StrView = FfiSlice<u8>;

/// Unique 128-bit component identifier (UUID-compatible).
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ComponentUuid {
    pub bytes: [u8; 16],
}

impl ComponentUuid {
    /// Deterministic 128-bit hash computed from a component name string using two
    /// independent 64-bit FNV-1a lanes with distinct basis constants.
    pub const fn from_name(name: &str) -> Self {
        let bytes = name.as_bytes();
        let mut h1: u64 = 0xcbf29ce484222325;
        let mut h2: u64 = 0x84222325cbf29ce4;
        let mut i = 0;
        while i < bytes.len() {
            h1 ^= bytes[i] as u64;
            h1 = h1.wrapping_mul(0x100000001b3);
            h2 ^= (bytes[i] as u64) ^ ((i as u64).wrapping_mul(0x9e3779b97f4a7c15));
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

/// FFI-safe component access mode declaration (`#[repr(transparent)]` `u32` newtype).
///
/// Using a single integer mode prevents contradictory states (such as simultaneous
/// `is_mutable: true` and `is_filter_only: true`) and avoids Rust `enum`/`bool` UB from C.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ComponentAccessMode(pub u32);

#[allow(non_upper_case_globals)]
impl ComponentAccessMode {
    /// Read-only column access (`&T`).
    pub const READ: Self = Self(0);
    /// Mutable column access (`&mut T`).
    pub const WRITE: Self = Self(1);
    /// Archetype filter-only (`With<T>`), no column pointer accessed.
    pub const FILTER_ONLY: Self = Self(2);

    pub const Read: Self = Self::READ;
    pub const Write: Self = Self::WRITE;
    pub const FilterOnly: Self = Self::FILTER_ONLY;

    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::FILTER_ONLY.0
    }

    #[inline]
    pub const fn is_mutable(self) -> bool {
        self.0 == Self::WRITE.0
    }

    #[inline]
    pub const fn is_filter_only(self) -> bool {
        self.0 == Self::FILTER_ONLY.0
    }
}

/// Component access descriptor passed in `SystemDescriptor::queries`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessDesc {
    /// Null-terminated UTF-8 C string identifying the registered component name.
    pub name: *const c_char,
    /// Access mode (`ComponentAccessMode::READ`, `WRITE`, or `FILTER_ONLY`).
    pub mode: ComponentAccessMode,
}

impl ComponentAccessDesc {
    #[inline]
    pub const fn read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::READ,
        }
    }

    #[inline]
    pub const fn write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::WRITE,
        }
    }

    #[inline]
    pub const fn filter_only(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::FILTER_ONLY,
        }
    }
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
pub type IterNextChunkFn = unsafe extern "C" fn(iter: *mut RawChunkIter) -> usize;

/// Obtain a read-only byte pointer to the start of column `index` in the current chunk.
/// Returns `null` if `index` is out of bounds or is a filter-only component.
pub type IterColumnReadonlyFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> *const u8;

/// Obtain a mutable byte pointer to the start of column `index` in the current chunk.
/// Marks `ComponentTicks` as changed for all entities in the chunk.
/// Returns `null` if the column was not declared with `ComponentAccessMode::WRITE`.
pub type IterColumnMutableFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize) -> *mut u8;

/// Explicitly mark row `row` in column `index` as changed.
pub type IterMarkChangedFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult;

/// Obtain a pointer to the contiguous `FfiEntity` array for the current chunk.
pub type IterEntitiesFn = unsafe extern "C" fn(iter: *const RawChunkIter) -> *const FfiEntity;

/// Return the byte stride (`size_of::<C>()`) of column `index`.
pub type IterColumnStrideFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> usize;

/// Dynamic system callback invoked by the host during schedule execution.
///
/// Returns `FfiResult::OK` on success, or `FfiResult::PANIC_CAUGHT` / error status if the
/// guest system faulted (allowing the host to isolate and disable the faulting system
/// without unwinding across foreign runtime boundaries).
pub type DynamicSystemCallback = unsafe extern "C" fn(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult;

/// Descriptor used by mods to register a `DynamicCSystem` with the host.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct SystemDescriptor {
    /// Size of this struct in bytes (`core::mem::size_of::<SystemDescriptor>() as u32`) for ABI forward compatibility.
    pub struct_size: u32,
    /// Null-terminated UTF-8 system name.
    pub name: *const c_char,
    /// Pointer to an array of `ComponentAccessDesc` of length `query_count`.
    pub queries: *const ComponentAccessDesc,
    /// Number of elements in `queries`.
    pub query_count: usize,
    /// System execution callback.
    pub callback: DynamicSystemCallback,
    /// Opaque guest-owned context pointer forwarded to `callback`.
    pub user_data: *mut c_void,
}

/// System registration function signature exposed in `HostApi`.
pub type RegisterSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult;

/// Symbol-resolver function pointer passed to the guest mod entry point (Godot GDExtension style).
pub type GetProcAddressFn = unsafe extern "C" fn(name: *const c_char) -> *const c_void;

/// Host API VTable passed to the guest mod during `bevy_abi_mod_init`.
///
/// Must have `'static` lifetime on the host so guest mods may store the pointer across frames.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct HostApi {
    /// Size of this struct in bytes (`core::mem::size_of::<HostApi>() as u32`).
    pub struct_size: u32,
    /// Packed host ABI version (`ABI_VERSION_PACKED`).
    pub abi_version: u32,
    /// Register a dynamic ECS system with the host schedule.
    pub register_system: RegisterSystemFn,
    /// Advance a `RawChunkIter` to the next table chunk.
    pub iter_next_chunk: IterNextChunkFn,
    /// Borrow a read-only column byte pointer from a `RawChunkIter`.
    pub iter_column_readonly: IterColumnReadonlyFn,
    /// Borrow a mutable column byte pointer from a `RawChunkIter` (marks chunk ticks changed).
    pub iter_column_mutable: IterColumnMutableFn,
    /// Mark an individual row's component tick changed.
    pub iter_mark_changed: IterMarkChangedFn,
    /// Borrow the contiguous `FfiEntity` column from a `RawChunkIter`.
    pub iter_entities: IterEntitiesFn,
    /// Query the byte stride of a column in a `RawChunkIter`.
    pub iter_column_stride: IterColumnStrideFn,
}

/// Mod initialization entry point function signature (`bevy_abi_mod_init`).
pub type ModInitFn = unsafe extern "C" fn(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult;

/// Mod teardown function signature (`bevy_abi_mod_shutdown`).
pub type ModShutdownFn = unsafe extern "C" fn(host_context: *mut c_void) -> FfiResult;

/// Optional ABI version query entry point (`bevy_abi_mod_abi_version`).
pub type ModAbiVersionFn = unsafe extern "C" fn() -> u32;

/// Marker trait for types whose layout is safe to expose across the modding boundary
/// via interior pointers (`<Struct>_ptr_<field>`) or contiguous `Vec<T>` slices.
///
/// # Safety
/// Implementing this trait asserts that `Self` either:
/// - Is a primitive C-compatible value type (`Copy + 'static` with no internal heap pointers), or
/// - Derives `ModExposable` and is accessed strictly through its generated host VTable accessors.
pub unsafe trait FfiLayout: Sized + 'static {}

macro_rules! impl_ffi_layout_primitives {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Primitive value type with no drop glue, allocator ownership, or invalid bit patterns
            // when manipulated through typed accessors.
            unsafe impl FfiLayout for $ty {}
        )*
    };
}

impl_ffi_layout_primitives!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, bool, FfiEntity
);

// SAFETY: Fixed-size arrays of `FfiLayout` elements have contiguous layout of `FfiLayout` items.
unsafe impl<T: FfiLayout, const N: usize> FfiLayout for [T; N] {}

/// Host-side symbol table entry mapping a symbol name to its static VTable or function pointer.
///
/// Intentionally a Rust metadata struct (not `#[repr(C)]`) used by `ExposableComponent::symbols()`
/// so the host's `ModRegistry` can populate its `get_proc_address` lookup table.
#[derive(Debug, Copy, Clone)]
pub struct SymbolEntry {
    pub name: &'static str,
    pub ptr: *const c_void,
}

// SAFETY: `SymbolEntry` only holds a `'static` string slice and an immutable static VTable/fn pointer.
unsafe impl Sync for SymbolEntry {}
// SAFETY: `SymbolEntry` only holds immutable static pointers safe to share across threads.
unsafe impl Send for SymbolEntry {}

/// Trait implemented by `#[derive(ModExposable)]` on host components.
///
/// Exposes the component's string name, deterministic UUID, layout (`size` and `align`),
/// static `#[repr(C)]` VTable, and C-ABI accessor symbol table for `ModRegistry` registration.
pub trait ExposableComponent: FfiLayout {
    /// The string name of the component (e.g., `"Position"` or `"game::Position"`).
    const COMPONENT_NAME: &'static str;

    /// Null-terminated C string literal of the component name.
    const COMPONENT_CSTR: &'static core::ffi::CStr;

    /// Deterministic 128-bit UUID for this component.
    const UUID: ComponentUuid;

    /// The generated `#[repr(C)]` VTable struct type (`<Struct>VTable`).
    type VTable: Copy + 'static;

    /// Returns a reference to the static VTable instance for this component.
    fn vtable() -> &'static Self::VTable;

    /// Returns a pointer to the static VTable instance for this component.
    #[inline]
    fn vtable_ptr() -> *const Self::VTable {
        Self::vtable() as *const Self::VTable
    }

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
    fn test_ffi_entity_bits_roundtrip() {
        let bits = 0x0000_0007_0000_002a_u64;
        let entity = FfiEntity::from_bits(bits);
        assert_eq!(entity.to_bits(), bits);
    }

    #[test]
    fn test_ffi_result_transparent_soundness() {
        assert!(FfiResult::OK.is_ok());
        assert!(!FfiResult::OK.is_err());
        assert!(FfiResult::PANIC_CAUGHT.is_err());

        // Arbitrary unknown integer from C must never cause UB
        let unknown_from_c = FfiResult(12345);
        assert!(unknown_from_c.is_err());
        assert_eq!(unknown_from_c.0, 12345);
    }

    #[test]
    fn test_component_access_mode_validity() {
        assert!(ComponentAccessMode::READ.is_valid());
        assert!(!ComponentAccessMode::READ.is_mutable());
        assert!(ComponentAccessMode::WRITE.is_valid());
        assert!(ComponentAccessMode::WRITE.is_mutable());
        assert!(ComponentAccessMode::FILTER_ONLY.is_valid());
        assert!(ComponentAccessMode::FILTER_ONLY.is_filter_only());
        assert!(!ComponentAccessMode(99).is_valid());
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

        let view: StrView = slice;
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
