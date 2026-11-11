#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `abi_contract`: Zero-Dependency C-ABI Contract for `bevy_abi_mod`
//!
//! This crate defines the foundational `#![no_std]`, zero-dependency C-ABI types, function
//! pointers, opaque handles, and handshake protocols connecting Bevy ECS host applications to
//! native dynamic guest mods (`.so`, `.dylib`, `.dll`).
//!
//! ## Core Architectural Principles
//!
//! ### 1. Zero Shared Rust ABI Assumptions
//! Rust does not have a stable ABI across compiler versions, build profiles (`debug` vs `release`),
//! or target flags. Passing complex Rust types (`bevy_ecs::world::World`, `bevy_ecs::system::Query`,
//! `Box<T>`, `Vec<T>`, `String`, or fat slice references `&[T]`) across a shared library boundary
//! leads to undefined behavior if the host and guest were compiled with differing compilers or flags.
//!
//! In `bevy_abi_mod`, the host and guest share **zero Rust types or allocator state**:
//! - All data exchanged across the boundary is `#[repr(C)]` or `#[repr(transparent)]`.
//! - Function pointers are strictly `unsafe extern "C" fn`.
//! - Arrays are passed as `FfiSlice<T>` (`{ ptr: *const T, len: usize }`) instead of Rust fat pointers.
//! - String views are passed as `StrView` (`FfiSlice<u8>`).
//!
//! ### 2. Memory & Allocator Isolation
//! - Memory allocated on the host must be freed by the host.
//! - Memory allocated on the guest must be freed by the guest.
//! - Passing owned heap collections (`Box`, `Vec`, `String`) across the boundary is strictly prohibited.
//! - When guest code inserts components via deferred commands, the host copies the raw bytes into
//!   host-managed memory before scheduling insertion, guaranteeing that unloading the guest `.cdylib`
//!   cannot cause use-after-free or memory corruption on the host.
//!
//! ### 3. Transparent Newtypes vs. Rust Enums
//! Rust enums have strict discriminant ranges. If foreign C, C++, or Zig code returns an unexpected
//! integer value into a Rust `enum`, it instantly creates undefined behavior.
//! To ensure sound FFI:
//! - Status codes ([`FfiResult`]) and access modes ([`ComponentAccessMode`]) are declared as
//!   `#[repr(transparent)] struct Type(pub u32)`.
//! - Any 32-bit bit pattern is sound in Rust, allowing graceful error detection without crashes.
//!
//! ### 4. Calling Conventions & Unwinding
//! Unwinding a panic across an `extern "C"` boundary is undefined behavior in Rust and may cause
//! immediate process aborts. All function pointers across the ABI are `unsafe extern "C" fn`.
//! Guest libraries are expected to contain panics within their own runtime via `catch_unwind` and
//! return [`FfiResult::PANIC_CAUGHT`] by value.
//!
//! ## Subsystem Map
//! 1. **Core Primitives**: [`FfiResult`], [`FfiBool`], [`FfiEntity`], [`FfiSlice`], [`StrView`], [`ComponentUuid`].
//! 2. **Query & Schedule Declarations**: [`ComponentAccessMode`], [`ComponentAccessDesc`], [`ScheduleLabelId`].
//! 3. **Archetype Traversal Handles**: [`RawChunkIter`], [`IterNextChunkFn`], [`IterColumnReadonlyFn`],
//!    [`IterColumnMutableFn`], [`IterMarkChangedFn`], [`IterEntitiesFn`], [`IterColumnStrideFn`], [`IterResetFn`].
//! 4. **Command Queue, Registration & Versioning**: [`RawCommandQueue`], [`DynamicSystemCallback`], [`QuerySpec`],
//!    [`MultiQuerySystemCallback`], [`MultiQuerySystemDescriptor`], [`ScheduledMultiQuerySystemDescriptor`],
//!    [`SystemDescriptor`], [`ScheduledSystemDescriptor`], [`HostApi`], [`ExtensibleStruct`],
//!    [`read_extensible_struct`], [`read_descriptor`], [`abi_version_is_compatible`],
//!    [`RegisterSystemFn`], [`RegisterMultiQuerySystemFn`], [`GetProcAddressFn`].
//! 5. **Dynamic Components & POD Traits**: [`ComponentDropFn`], [`ModComponentDescriptor`],
//!    [`DynamicComponentDescriptor`], [`RegisterComponentFn`], [`FfiLayout`], [`AbiPod`],
//!    [`SymbolEntry`], [`ExposableComponent`].

use core::ffi::{c_char, c_void};

/// ABI-safe 8-bit boolean flag (`0` = `false`, non-zero = `true`).
///
/// Rust's `bool` only permits byte values `0` and `1`; passing any other byte (`2..=255`) from
/// C/C++/Zig into an `extern "C"` Rust `bool` parameter is immediate undefined behavior.
/// All boolean values crossing the C-ABI use `FfiBool` (`u8`) instead.
pub type FfiBool = u8;

/// Canonical `false` value for [`FfiBool`].
pub const FFI_BOOL_FALSE: FfiBool = 0;
/// Canonical `true` value for [`FfiBool`].
pub const FFI_BOOL_TRUE: FfiBool = 1;

/// Converts an [`FfiBool`] byte (`0` = `false`, any non-zero byte = `true`) into a Rust `bool`
/// without undefined behavior on non-canonical foreign truthy values (`2..=255`).
#[inline]
pub const fn ffi_bool_to_rust(flag: FfiBool) -> bool {
    flag != FFI_BOOL_FALSE
}

/// Converts a Rust `bool` into a canonical [`FfiBool`] (`0` or `1`).
#[inline]
pub const fn rust_bool_to_ffi(val: bool) -> FfiBool {
    if val {
        FFI_BOOL_TRUE
    } else {
        FFI_BOOL_FALSE
    }
}

/// Major version of the ABI contract. Incremented for breaking ABI layout or signature changes.
pub const ABI_VERSION_MAJOR: u32 = 0;
/// Minor version of the ABI contract. Incremented for backward-compatible symbol or capability additions.
pub const ABI_VERSION_MINOR: u32 = 1;
/// Patch version of the ABI contract. Incremented for backward-compatible internal bug fixes.
pub const ABI_VERSION_PATCH: u32 = 0;

/// Packs `(major, minor, patch)` into a 32-bit ABI version word: `(major << 24) | (minor << 16) | patch`.
#[inline]
pub const fn pack_abi_version(major: u32, minor: u32, patch: u32) -> u32 {
    ((major & 0xFF) << 24) | ((minor & 0xFF) << 16) | (patch & 0xFFFF)
}

/// Extracts the 8-bit major version from a packed ABI version word.
#[inline]
pub const fn abi_version_major(packed: u32) -> u32 {
    (packed >> 24) & 0xFF
}

/// Extracts the 8-bit minor version from a packed ABI version word.
#[inline]
pub const fn abi_version_minor(packed: u32) -> u32 {
    (packed >> 16) & 0xFF
}

/// Extracts the 16-bit patch version from a packed ABI version word.
#[inline]
pub const fn abi_version_patch(packed: u32) -> u32 {
    packed & 0xFFFF
}

/// Returns `true` if a guest mod compiled against `guest_packed` is semver-compatible with a
/// host running `host_packed`.
///
/// Compatibility requires identical major versions (`host_major == guest_major`) and a guest minor
/// version no newer than the host's (`guest_minor <= host_minor`). Patch differences are always
/// compatible in both directions.
#[inline]
pub const fn abi_version_is_compatible(host_packed: u32, guest_packed: u32) -> bool {
    abi_version_major(host_packed) == abi_version_major(guest_packed)
        && abi_version_minor(guest_packed) <= abi_version_minor(host_packed)
}

/// Packed 32-bit ABI version: `(MAJOR << 24) | (MINOR << 16) | PATCH`.
///
/// Bit packing allows host and guest to negotiate compatibility with a single 32-bit integer comparison.
///
/// # Example
///
/// ```
/// use abi_contract::{
///     abi_version_is_compatible, pack_abi_version, ABI_VERSION_MAJOR, ABI_VERSION_MINOR,
///     ABI_VERSION_PACKED, ABI_VERSION_PATCH,
/// };
///
/// assert_eq!((ABI_VERSION_PACKED >> 24) & 0xFF, ABI_VERSION_MAJOR);
/// assert_eq!((ABI_VERSION_PACKED >> 16) & 0xFF, ABI_VERSION_MINOR);
/// assert_eq!(ABI_VERSION_PACKED & 0xFFFF, ABI_VERSION_PATCH);
/// assert!(abi_version_is_compatible(pack_abi_version(0, 2, 0), ABI_VERSION_PACKED));
/// ```
pub const ABI_VERSION_PACKED: u32 =
    pack_abi_version(ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH);

/// FFI-safe status code returned across the C-ABI boundary.
///
/// Represented as a `#[repr(transparent)]` `u32` newtype rather than a Rust `enum`
/// so that arbitrary integer values returned by foreign C/C++ code can never trigger
/// invalid-discriminant undefined behavior in Rust.
///
/// # Example
///
/// ```
/// use abi_contract::FfiResult;
///
/// let res = FfiResult::OK;
/// assert!(res.is_ok());
/// assert!(!res.is_err());
///
/// match res {
///     FfiResult::Ok => {},
///     FfiResult::PanicCaught => panic!("caught panic"),
///     _ => {},
/// }
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiResult(pub u32);

#[allow(non_upper_case_globals)]
impl FfiResult {
    /// Operation succeeded without error.
    pub const OK: Self = Self(0);
    /// Component name or UUID was not found in the host registry.
    pub const INVALID_COMPONENT: Self = Self(1);
    /// Requested query access declarations conflict with scheduler borrow rules.
    pub const ACCESS_CONFLICT: Self = Self(2);
    /// Mod system panicked; caught within the guest `.cdylib` without unwinding into the host.
    pub const PANIC_CAUGHT: Self = Self(3);
    /// Null pointer was passed for a mandatory reference or slice argument.
    pub const NULL_POINTER: Self = Self(4);
    /// String view contains non-UTF-8 bytes.
    pub const INVALID_UTF8: Self = Self(5);
    /// Unauthorized operation attempted (e.g. attempting mutable access on a read-only column).
    pub const PERMISSION_DENIED: Self = Self(6);
    /// ABI version mismatch between host and mod.
    pub const INVALID_VERSION: Self = Self(7);
    /// Unexpected internal host or guest error.
    pub const INTERNAL_ERROR: Self = Self(99);

    // PascalCase aliases for ergonomic pattern matching
    /// Alias for [`Self::OK`].
    pub const Ok: Self = Self::OK;
    /// Alias for [`Self::INVALID_COMPONENT`].
    pub const InvalidComponent: Self = Self::INVALID_COMPONENT;
    /// Alias for [`Self::ACCESS_CONFLICT`].
    pub const AccessConflict: Self = Self::ACCESS_CONFLICT;
    /// Alias for [`Self::PANIC_CAUGHT`].
    pub const PanicCaught: Self = Self::PANIC_CAUGHT;
    /// Alias for [`Self::NULL_POINTER`].
    pub const NullPointer: Self = Self::NULL_POINTER;
    /// Alias for [`Self::INVALID_UTF8`].
    pub const InvalidUtf8: Self = Self::INVALID_UTF8;
    /// Alias for [`Self::PERMISSION_DENIED`].
    pub const PermissionDenied: Self = Self::PERMISSION_DENIED;
    /// Alias for [`Self::INVALID_VERSION`].
    pub const InvalidVersion: Self = Self::INVALID_VERSION;
    /// Alias for [`Self::INTERNAL_ERROR`].
    pub const InternalError: Self = Self::INTERNAL_ERROR;

    /// Returns `true` if this result indicates success (`OK`).
    #[inline]
    pub const fn is_ok(self) -> bool {
        self.0 == Self::OK.0
    }

    /// Returns `true` if this result indicates an error.
    #[inline]
    pub const fn is_err(self) -> bool {
        self.0 != Self::OK.0
    }
}

/// Opaque 64-bit entity identifier matching `bevy_ecs::entity::Entity::to_bits()`.
///
/// Foreign code must treat this as an opaque token (never assuming index vs generation bit widths).
/// In Bevy 0.20+, `EntityIndex` is backed by a `NonMaxU32` stored on the wire via `NonZero<u32>`
/// bit encoding, so `0` (`FfiEntity::NULL`) is **never** a valid Bevy `Entity`
/// (`Entity::try_from_bits(0)` is guaranteed `None`), whereas `u64::MAX` is a valid entity bit pattern
/// and Bevy's `Entity::PLACEHOLDER.to_bits()` is `0x1`. Always use [`FfiEntity::NULL`] (`0`) as the
/// invalid/null entity sentinel across the C-ABI.
///
/// # Example
///
/// ```
/// use abi_contract::FfiEntity;
///
/// let entity = FfiEntity::from_bits(42);
/// assert_eq!(entity.to_bits(), 42);
/// assert!(!entity.is_null());
/// assert!(FfiEntity::NULL.is_null());
/// assert_eq!(FfiEntity::default(), FfiEntity::NULL);
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiEntity(pub u64);

impl FfiEntity {
    /// Canonical null / invalid entity sentinel (`0`).
    ///
    /// Guaranteed never to collide with any allocated Bevy `Entity` (`Entity::try_from_bits(0) == None`).
    pub const NULL: Self = Self(0);

    /// Legacy placeholder constant (`u64::MAX`).
    ///
    /// **Deprecated**: In Bevy 0.20+, `u64::MAX` does **not** match `Entity::PLACEHOLDER.to_bits()` (`0x1`)
    /// and decodes as a valid `Entity` (`index = 0`). Use [`FfiEntity::NULL`] (`0`) instead.
    #[deprecated(
        since = "0.1.1",
        note = "Use `FfiEntity::NULL` (`0`). `u64::MAX` does not match Bevy 0.20's `Entity::PLACEHOLDER` (`0x1`)."
    )]
    pub const PLACEHOLDER: Self = Self(u64::MAX);

    /// Constructs an `FfiEntity` from raw 64-bit bits.
    #[inline]
    pub const fn from_bits(bits: u64) -> Self {
        Self(bits)
    }

    /// Extracts the raw 64-bit integer representation of this entity.
    #[inline]
    pub const fn to_bits(self) -> u64 {
        self.0
    }

    /// Returns `true` if this entity equals the invalid [`FfiEntity::NULL`] sentinel (`0`).
    #[inline]
    pub const fn is_null(self) -> bool {
        self.0 == Self::NULL.0
    }
}

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
/// use abi_contract::{FfiSlice, StrView};
///
/// let values = [10u32, 20, 30];
/// let slice = FfiSlice::from_slice(&values);
/// assert_eq!(slice.len, 3);
/// assert!(!slice.is_empty());
/// unsafe {
///     assert_eq!(slice.as_slice(), &[10, 20, 30]);
/// }
///
/// let text = "bevy_abi_mod";
/// let view = StrView::from_str(text);
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

/// Unique 128-bit component identifier (UUID-compatible).
///
/// Computed deterministically at compile-time via 2-lane 64-bit FNV-1a hashing.
///
/// # Example
///
/// ```
/// use abi_contract::ComponentUuid;
///
/// let id1 = ComponentUuid::from_name("Position");
/// let id2 = ComponentUuid::from_name("Position");
/// let id3 = ComponentUuid::from_name("Velocity");
/// assert_eq!(id1, id2);
/// assert_ne!(id1, id3);
/// ```
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct ComponentUuid {
    /// 16-byte raw UUID representation.
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
///
/// # Example
///
/// ```
/// use abi_contract::{ComponentAccessMode, ComponentAccessDesc};
///
/// let mode = ComponentAccessMode::READ;
/// assert!(mode.is_valid());
/// assert!(!mode.is_mutable());
///
/// let desc = ComponentAccessDesc::read(c"Position".as_ptr());
/// assert_eq!(desc.mode, ComponentAccessMode::READ);
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ComponentAccessMode(pub u32);

#[allow(non_upper_case_globals)]
impl ComponentAccessMode {
    /// Read-only column access (`&T`). Multiple systems can access concurrently.
    pub const READ: Self = Self(0);
    /// Exclusive write access (`&mut T`). Triggers Bevy change ticks.
    pub const WRITE: Self = Self(1);
    /// Archetype presence filter (`With<T>`). Iterates entities without reading column memory.
    pub const FILTER_ONLY: Self = Self(2);
    /// Archetype absence filter (`Without<T>`). Proves query disjointness for scheduler concurrency.
    pub const WITHOUT: Self = Self(3);
    /// Optional read-only column access (`Option<&T>`). Matches archetypes with or without `T`;
    /// `iter_column_readonly` returns non-null when present or `null` when absent.
    pub const OPTIONAL_READ: Self = Self(4);
    /// Optional exclusive write access (`Option<&mut T>`). Matches archetypes with or without `T`;
    /// `iter_column_mutable` returns non-null and marks ticks when present or `null` when absent.
    pub const OPTIONAL_WRITE: Self = Self(5);
    /// Row-level change-tick filter (`Changed<T>`) with read-only column access.
    /// Yields contiguous runs of entities whose `T` change tick is newer than the system's `last_run`.
    pub const CHANGED: Self = Self(6);
    /// Row-level added-tick filter (`Added<T>`) with read-only column access.
    /// Yields contiguous runs of entities whose `T` added tick is newer than the system's `last_run`.
    pub const ADDED: Self = Self(7);
    /// Shared read-only access to a Bevy singleton `Resource` (`Res<R>`).
    pub const RESOURCE_READ: Self = Self(8);
    /// Exclusive mutable access to a Bevy singleton `Resource` (`ResMut<R>`).
    pub const RESOURCE_WRITE: Self = Self(9);

    /// Alias for [`Self::READ`].
    pub const Read: Self = Self::READ;
    /// Alias for [`Self::WRITE`].
    pub const Write: Self = Self::WRITE;
    /// Alias for [`Self::FILTER_ONLY`].
    pub const FilterOnly: Self = Self::FILTER_ONLY;
    /// Alias for [`Self::WITHOUT`].
    pub const Without: Self = Self::WITHOUT;
    /// Alias for [`Self::OPTIONAL_READ`].
    pub const OptionalRead: Self = Self::OPTIONAL_READ;
    /// Alias for [`Self::OPTIONAL_WRITE`].
    pub const OptionalWrite: Self = Self::OPTIONAL_WRITE;
    /// Alias for [`Self::CHANGED`].
    pub const Changed: Self = Self::CHANGED;
    /// Alias for [`Self::ADDED`].
    pub const Added: Self = Self::ADDED;
    /// Alias for [`Self::RESOURCE_READ`].
    pub const ResourceRead: Self = Self::RESOURCE_READ;
    /// Alias for [`Self::RESOURCE_WRITE`].
    pub const ResourceWrite: Self = Self::RESOURCE_WRITE;

    /// Returns `true` if this mode is one of the recognized variants (`0..=9`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::RESOURCE_WRITE.0
    }

    /// Returns `true` if this access mode requires exclusive mutable access (`WRITE`, `OPTIONAL_WRITE`, or `RESOURCE_WRITE`).
    #[inline]
    pub const fn is_mutable(self) -> bool {
        self.0 == Self::WRITE.0
            || self.0 == Self::OPTIONAL_WRITE.0
            || self.0 == Self::RESOURCE_WRITE.0
    }

    /// Returns `true` if this access mode is an archetype presence filter (`With<T>`).
    #[inline]
    pub const fn is_filter_only(self) -> bool {
        self.0 == Self::FILTER_ONLY.0
    }

    /// Returns `true` if this access mode is an archetype absence filter (`Without<T>`).
    #[inline]
    pub const fn is_without(self) -> bool {
        self.0 == Self::WITHOUT.0
    }

    /// Returns `true` if this access mode is optional (`Option<&T>` or `Option<&mut T>`).
    #[inline]
    pub const fn is_optional(self) -> bool {
        self.0 == Self::OPTIONAL_READ.0 || self.0 == Self::OPTIONAL_WRITE.0
    }

    /// Returns `true` if this access mode filters rows by component ticks (`Changed<T>` or `Added<T>`).
    #[inline]
    pub const fn is_tick_filter(self) -> bool {
        self.0 == Self::CHANGED.0 || self.0 == Self::ADDED.0
    }

    /// Returns `true` if this access mode targets a singleton Bevy `Resource` (`Res<R>` or `ResMut<R>`).
    #[inline]
    pub const fn is_resource(self) -> bool {
        self.0 == Self::RESOURCE_READ.0 || self.0 == Self::RESOURCE_WRITE.0
    }
}

/// Component access descriptor passed in `SystemDescriptor::queries`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ComponentAccessDesc {
    /// Null-terminated UTF-8 C string identifying the registered component or resource name.
    pub name: *const c_char,
    /// Access mode (`READ`, `WRITE`, `FILTER_ONLY`, `WITHOUT`, `OPTIONAL_*`, `CHANGED`, `ADDED`, `RESOURCE_*`).
    pub mode: ComponentAccessMode,
}

impl ComponentAccessDesc {
    /// Creates a read-only access descriptor for the specified component name.
    #[inline]
    pub const fn read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::READ,
        }
    }

    /// Creates an exclusive write access descriptor for the specified component name.
    #[inline]
    pub const fn write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::WRITE,
        }
    }

    /// Creates an archetype presence filter (`With<T>`) descriptor.
    #[inline]
    pub const fn filter_only(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::FILTER_ONLY,
        }
    }

    /// Creates an archetype absence filter (`Without<T>`) descriptor.
    #[inline]
    pub const fn without(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::WITHOUT,
        }
    }

    /// Creates an optional read-only (`Option<&T>`) access descriptor.
    #[inline]
    pub const fn optional_read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::OPTIONAL_READ,
        }
    }

    /// Creates an optional mutable (`Option<&mut T>`) access descriptor.
    #[inline]
    pub const fn optional_write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::OPTIONAL_WRITE,
        }
    }

    /// Creates a per-row change-tick filter (`Changed<T>`) descriptor.
    #[inline]
    pub const fn changed(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::CHANGED,
        }
    }

    /// Creates a per-row added-tick filter (`Added<T>`) descriptor.
    #[inline]
    pub const fn added(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::ADDED,
        }
    }

    /// Creates a shared resource read (`Res<R>`) descriptor.
    #[inline]
    pub const fn resource_read(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::RESOURCE_READ,
        }
    }

    /// Creates an exclusive resource write (`ResMut<R>`) descriptor.
    #[inline]
    pub const fn resource_write(name: *const c_char) -> Self {
        Self {
            name,
            mode: ComponentAccessMode::RESOURCE_WRITE,
        }
    }
}

/// Opaque C handle representing an active Archetype Table chunk iterator.
///
/// The `_private: [u8; 0]` pattern creates a zero-sized, non-constructible opaque type
/// that prevents foreign code from inspecting or corrupting internal host iteration buffers.
#[repr(C)]
pub struct RawChunkIter {
    _private: [u8; 0],
}

/// Opaque C handle representing a deferred command queue passed to dynamic systems.
///
/// The `_private: [u8; 0]` pattern creates a zero-sized, non-constructible opaque type
/// that prevents foreign code from inspecting or corrupting internal host command queue buffers.
#[repr(C)]
pub struct RawCommandQueue {
    _private: [u8; 0],
}

/// Advances the chunk iterator to the next matching archetype table.
///
/// Returns `0` when exhausted, or the `entity_count` for the active chunk.
///
/// # Safety
/// `iter` must be a valid, non-null pointer to an active [`RawChunkIter`].
pub type IterNextChunkFn = unsafe extern "C" fn(iter: *mut RawChunkIter) -> usize;

/// Obtains a read-only byte pointer to the start of column `index` in the current chunk.
///
/// Returns `null` if `index` is out of bounds or is a filter-only component.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`] with `entity_count > 0`.
/// - `index` must be strictly less than the number of queries declared for this system.
/// - The column must not be filter-only or without.
pub type IterColumnReadonlyFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> *const u8;

/// Obtains a mutable byte pointer to the start of column `index` in the current chunk.
///
/// Marks `ComponentTicks` as changed for all entities in the chunk.
/// Returns `null` if the column was not declared with `ComponentAccessMode::WRITE`.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`] with `entity_count > 0`.
/// - `index` must have been declared with [`ComponentAccessMode::WRITE`].
pub type IterColumnMutableFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize) -> *mut u8;

/// Explicitly marks row `row` in column `index` as changed.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`].
/// - `row` must be strictly less than the current chunk entity count.
/// - `index` must have been declared with [`ComponentAccessMode::WRITE`].
pub type IterMarkChangedFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult;

/// Obtains a pointer to the contiguous array of `FfiEntity` identifiers for the current chunk.
///
/// # Safety
/// `iter` must be an active [`RawChunkIter`]; returns a pointer to `[FfiEntity; entity_count]`.
pub type IterEntitiesFn = unsafe extern "C" fn(iter: *const RawChunkIter) -> *const FfiEntity;

/// Returns the byte stride (`size_of::<T>()`) of column `index` for array indexing.
///
/// # Safety
/// `iter` must be a valid pointer to a [`RawChunkIter`].
pub type IterColumnStrideFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> usize;

/// Resets a `RawChunkIter` cursor back to its initial matched archetype table so inner queries
/// can be re-traversed inside nested query loops with zero heap allocations.
///
/// # Safety
/// `iter` must be a valid pointer to a [`RawChunkIter`].
pub type IterResetFn = unsafe extern "C" fn(iter: *mut RawChunkIter) -> FfiResult;

/// Obtains a read-only byte pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Returns `null` if `iter` or `resource_name` is null, if the resource was not declared with
/// [`ComponentAccessMode::RESOURCE_READ`] or [`ComponentAccessMode::RESOURCE_WRITE`] on this system,
/// or if the resource is not currently inserted in the `World`.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`RawChunkIter`] passed to the system callback.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string.
pub type IterResourceReadonlyFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, resource_name: *const c_char) -> *const u8;

/// Obtains an exclusive mutable byte pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Marks the resource's Bevy change tick as changed (`Res<R>::is_changed()` becomes `true`).
/// Returns `null` if `iter` or `resource_name` is null, if the resource was not declared strictly with
/// [`ComponentAccessMode::RESOURCE_WRITE`] on this system, or if the resource is not present in the `World`.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`RawChunkIter`] passed to the system callback.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string.
/// - Caller must not create aliasing references to the same resource simultaneously.
pub type IterResourceMutableFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, resource_name: *const c_char) -> *mut u8;

/// Queues deferred removal of component `component_name` from `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string.
pub type CmdRemoveComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult;

/// Queues deferred insertion of host-constructed `T::default()` for component `component_name` on `entity`.
///
/// Enables guest mods to attach opaque non-POD host components (such as `String`- or `Vec`-bearing
/// components registered via `register_mod_component_with_default`) without allocator crossover,
/// and subsequently mutate their fields via VTable setters on the next frame.
///
/// Returns [`FfiResult::PERMISSION_DENIED`] if the component was not registered with a `Default` constructor.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string.
pub type CmdInsertDefaultFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult;

/// Optional guest run-condition predicate evaluated before a dynamic system executes.
///
/// Returns [`FFI_BOOL_FALSE`] (`0`) to skip system execution for the current tick (preserving
/// `last_run` so `Changed<T>` and `Added<T>` ticks accumulated while skipped are still observed
/// when the condition later returns non-zero), or any non-zero [`FfiBool`] value to execute.
///
/// # Safety
/// - Must contain any internal panics; unwinding across the `extern "C"` boundary is undefined behavior.
pub type RunConditionCallback = unsafe extern "C" fn(user_data: *mut c_void) -> FfiBool;

/// Dynamic system callback invoked by the host during schedule execution.
///
/// # Safety
/// - Invoked by the host ECS scheduler.
/// - The guest system must contain any internal panics and return [`FfiResult::PANIC_CAUGHT`].
/// - Handles passed in `iter` and `commands` are valid only for the duration of the callback.
pub type DynamicSystemCallback = unsafe extern "C" fn(
    iter: *mut RawChunkIter,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult;

/// Specification of one query clause within a multi-query dynamic system.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct QuerySpec {
    /// Pointer to an array of `ComponentAccessDesc` of length `query_count`.
    pub queries: *const ComponentAccessDesc,
    /// Number of elements in `queries`.
    pub query_count: usize,
}

/// Callback signature for a dynamic system executing multiple queries (e.g., nested queries).
///
/// # Safety
/// - `iters` points to an array of `*mut RawChunkIter` of length `iter_count`.
/// - Guest must contain panics and return [`FfiResult::PANIC_CAUGHT`].
pub type MultiQuerySystemCallback = unsafe extern "C" fn(
    iters: *const *mut RawChunkIter,
    iter_count: usize,
    commands: *mut RawCommandQueue,
    user_data: *mut c_void,
) -> FfiResult;

/// FFI-safe Bevy schedule identifier (`#[repr(transparent)]` `u32` newtype).
///
/// Defaults to [`ScheduleLabelId::UPDATE`] (`0`), so older `SystemDescriptor` instances zero-extended
/// via [`read_extensible_struct`] automatically run in Bevy's `Update` schedule.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct ScheduleLabelId(pub u32);

#[allow(non_upper_case_globals)]
impl ScheduleLabelId {
    /// Main per-frame `Update` schedule (`0`, default).
    pub const UPDATE: Self = Self(0);
    /// One-shot `Startup` schedule (`1`).
    pub const STARTUP: Self = Self(1);
    /// Per-frame `PreUpdate` schedule (`2`).
    pub const PRE_UPDATE: Self = Self(2);
    /// Per-frame `PostUpdate` schedule (`3`).
    pub const POST_UPDATE: Self = Self(3);
    /// Fixed-timestep `FixedUpdate` schedule (`4`).
    pub const FIXED_UPDATE: Self = Self(4);
    /// End-of-frame `Last` schedule (`5`).
    pub const LAST: Self = Self(5);

    /// Alias for [`Self::UPDATE`].
    pub const Update: Self = Self::UPDATE;
    /// Alias for [`Self::STARTUP`].
    pub const Startup: Self = Self::STARTUP;
    /// Alias for [`Self::PRE_UPDATE`].
    pub const PreUpdate: Self = Self::PRE_UPDATE;
    /// Alias for [`Self::POST_UPDATE`].
    pub const PostUpdate: Self = Self::POST_UPDATE;
    /// Alias for [`Self::FIXED_UPDATE`].
    pub const FixedUpdate: Self = Self::FIXED_UPDATE;
    /// Alias for [`Self::LAST`].
    pub const Last: Self = Self::LAST;

    /// Returns `true` if this schedule label ID is one of the recognized built-in variants (`0..=5`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 <= Self::LAST.0
    }
}

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
    /// Minimum valid `struct_size` in bytes (the base v0.1 layout size).
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
        core::ptr::copy_nonoverlapping(ptr, slot.as_mut_ptr() as *mut u8, copy_len);
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
    let declared_size = unsafe { core::ptr::read_unaligned(ptr as *const u32) } as usize;
    // SAFETY: Forwarding `ptr` and its declared `struct_size` under caller's readability contract.
    unsafe { read_extensible_struct::<T>(ptr as *const u8, declared_size) }
}

/// Descriptor used by mods to register a multi-query `DynamicCSystem` via `register_multi_query_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct MultiQuerySystemDescriptor {
    /// Size of this struct in bytes (`core::mem::size_of::<MultiQuerySystemDescriptor>() as u32`).
    pub struct_size: u32,
    /// Null-terminated UTF-8 system name.
    pub name: *const c_char,
    /// Pointer to an array of `QuerySpec` of length `query_spec_count`.
    pub query_specs: *const QuerySpec,
    /// Number of query clauses in `query_specs`.
    pub query_spec_count: usize,
    /// Multi-query system execution callback.
    pub callback: MultiQuerySystemCallback,
    /// Opaque guest-owned context pointer forwarded to `callback`.
    pub user_data: *mut c_void,
}

impl MultiQuerySystemDescriptor {
    /// Minimum valid v0.1 descriptor size in bytes (`48` on 64-bit, `24` on 32-bit).
    pub const MIN_V1_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a v0.1 multi-query system descriptor.
    #[inline]
    pub const fn new(
        name: *const c_char,
        query_specs: *const QuerySpec,
        query_spec_count: usize,
        callback: MultiQuerySystemCallback,
        user_data: *mut c_void,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name,
            query_specs,
            query_spec_count,
            callback,
            user_data,
        }
    }

    /// Promotes this descriptor into a [`ScheduledMultiQuerySystemDescriptor`] targeting `schedule`.
    #[inline]
    pub const fn with_schedule(
        self,
        schedule: ScheduleLabelId,
    ) -> ScheduledMultiQuerySystemDescriptor {
        ScheduledMultiQuerySystemDescriptor::from_base(self, schedule)
    }

    /// Promotes this descriptor into a [`ConditionalMultiQuerySystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalMultiQuerySystemDescriptor {
        ConditionalMultiQuerySystemDescriptor::from_scheduled(
            ScheduledMultiQuerySystemDescriptor::from_base(self, ScheduleLabelId::UPDATE),
            Some(run_if),
            run_if_user_data,
        )
    }

    /// Validates `struct_size` and mandatory pointer invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if self.name.is_null() || (self.query_spec_count > 0 && self.query_specs.is_null()) {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `MultiQuerySystemDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and
// `validate_raw_slot` verifies the non-nullable `callback` pointer before `assume_init()`.
unsafe impl ExtensibleStruct for MultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let cb_offset = core::mem::offset_of!(Self, callback);
        // SAFETY: `cb_offset + size_of::<usize>() <= size_of::<Self>()` inside `slot_ptr`.
        let cb_addr = unsafe {
            core::ptr::read_unaligned((slot_ptr as *const u8).add(cb_offset) as *const usize)
        };
        if cb_addr == 0 {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

/// Extended multi-query system descriptor adding Bevy schedule selection and intra-mod ordering.
///
/// Layout-prefixed by [`MultiQuerySystemDescriptor`] at offset `0`, so older v0.1 descriptors
/// zero-extend cleanly into `ScheduledMultiQuerySystemDescriptor` (`schedule = UPDATE`, empty
/// `before`/`after` arrays), and newer guests may pass [`Self::as_base_ptr`] directly to
/// [`RegisterMultiQuerySystemFn`].
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ScheduledMultiQuerySystemDescriptor {
    /// Base v0.1 multi-query descriptor header (`base.struct_size` set to `size_of::<Self>() as u32`).
    pub base: MultiQuerySystemDescriptor,
    /// Target Bevy schedule (`ScheduleLabelId::UPDATE` = `0` when zero-extended).
    pub schedule: ScheduleLabelId,
    /// Reserved flags word (`0` by default).
    pub flags: u32,
    /// Optional array of NUL-terminated system names this system must run after.
    pub after: *const *const c_char,
    /// Number of entries in `after`.
    pub after_count: usize,
    /// Optional array of NUL-terminated system names this system must run before.
    pub before: *const *const c_char,
    /// Number of entries in `before`.
    pub before_count: usize,
}

/// Alias for [`ScheduledMultiQuerySystemDescriptor`].
pub type MultiQuerySystemDescriptorV2 = ScheduledMultiQuerySystemDescriptor;

impl ScheduledMultiQuerySystemDescriptor {
    /// Minimum valid descriptor size (`MultiQuerySystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = MultiQuerySystemDescriptor::MIN_V1_SIZE;

    /// Wraps a base [`MultiQuerySystemDescriptor`] with a target [`ScheduleLabelId`].
    #[inline]
    pub const fn from_base(
        mut base: MultiQuerySystemDescriptor,
        schedule: ScheduleLabelId,
    ) -> Self {
        base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            base,
            schedule,
            flags: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
        }
    }

    /// Declares NUL-terminated system names that this system must run after.
    #[inline]
    pub const fn with_after(mut self, after: *const *const c_char, after_count: usize) -> Self {
        self.after = after;
        self.after_count = after_count;
        self
    }

    /// Declares NUL-terminated system names that this system must run before.
    #[inline]
    pub const fn with_before(mut self, before: *const *const c_char, before_count: usize) -> Self {
        self.before = before;
        self.before_count = before_count;
        self
    }

    /// Promotes this scheduled descriptor into a [`ConditionalMultiQuerySystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalMultiQuerySystemDescriptor {
        ConditionalMultiQuerySystemDescriptor::from_scheduled(self, Some(run_if), run_if_user_data)
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterMultiQuerySystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const MultiQuerySystemDescriptor {
        &self.base
    }

    /// Validates base descriptor fields, schedule label validity, and ordering array pointers.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        let base_res = self.base.validate();
        if base_res.is_err() {
            return base_res;
        }
        if !self.schedule.is_valid() {
            return FfiResult::INVALID_VERSION;
        }
        if (self.after_count > 0 && self.after.is_null())
            || (self.before_count > 0 && self.before.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `ScheduledMultiQuerySystemDescriptor` starts with `base: MultiQuerySystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`)
// are valid when zero-filled, and `validate_raw_slot` checks `base.callback != 0`.
unsafe impl ExtensibleStruct for ScheduledMultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `base` is at byte offset 0 of `Self`.
        unsafe {
            MultiQuerySystemDescriptor::validate_raw_slot(
                slot_ptr as *const MultiQuerySystemDescriptor,
            )
        }
    }
}

/// Extended v3 multi-query system descriptor adding an optional `run_if` condition callback.
///
/// Layout-prefixed by [`ScheduledMultiQuerySystemDescriptor`] at offset `0` (`104` bytes on 64-bit,
/// `56` bytes on 32-bit). Older v1 (`48`B) and v2 (`88`B) descriptors zero-extend cleanly into
/// `ConditionalMultiQuerySystemDescriptor` (`run_if = None`, `run_if_user_data = null`).
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ConditionalMultiQuerySystemDescriptor {
    /// Scheduled v2 multi-query system descriptor prefix (`sched.base.struct_size` = `size_of::<Self>() as u32`).
    pub sched: ScheduledMultiQuerySystemDescriptor,
    /// Optional run-condition predicate (`None` / `NULL` means execute unconditionally).
    pub run_if: Option<RunConditionCallback>,
    /// Opaque guest-owned context pointer forwarded to `run_if`.
    pub run_if_user_data: *mut c_void,
}

/// Alias for [`ConditionalMultiQuerySystemDescriptor`].
pub type MultiQuerySystemDescriptorV3 = ConditionalMultiQuerySystemDescriptor;

impl ConditionalMultiQuerySystemDescriptor {
    /// Minimum valid descriptor size (`MultiQuerySystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = MultiQuerySystemDescriptor::MIN_V1_SIZE;

    /// Wraps a [`ScheduledMultiQuerySystemDescriptor`] with an optional `run_if` predicate.
    #[inline]
    pub const fn from_scheduled(
        mut sched: ScheduledMultiQuerySystemDescriptor,
        run_if: Option<RunConditionCallback>,
        run_if_user_data: *mut c_void,
    ) -> Self {
        sched.base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            sched,
            run_if,
            run_if_user_data,
        }
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterMultiQuerySystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const MultiQuerySystemDescriptor {
        &self.sched.base
    }

    /// Validates base, schedule, and ordering invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        self.sched.validate()
    }
}

// SAFETY: `ConditionalMultiQuerySystemDescriptor` starts with `sched: ScheduledMultiQuerySystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`,
// `run_if: Option<...>`, `run_if_user_data`) are valid when zero-filled, and `validate_raw_slot` checks `callback != 0`.
unsafe impl ExtensibleStruct for ConditionalMultiQuerySystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `sched.base` is at byte offset 0 of `Self`.
        unsafe {
            MultiQuerySystemDescriptor::validate_raw_slot(
                slot_ptr as *const MultiQuerySystemDescriptor,
            )
        }
    }
}

/// Multi-query system registration function signature resolvable via `GetProcAddressFn`.
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterMultiQuerySystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const MultiQuerySystemDescriptor,
) -> FfiResult;

/// Explicit conditional multi-query system registration function signature resolvable via `GetProcAddressFn`.
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterConditionalMultiQuerySystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ConditionalMultiQuerySystemDescriptor,
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

impl SystemDescriptor {
    /// Minimum valid v0.1 descriptor size in bytes (`48` on 64-bit, `24` on 32-bit).
    pub const MIN_V1_SIZE: usize = core::mem::size_of::<Self>();

    /// Constructs a v0.1 `SystemDescriptor`.
    #[inline]
    pub const fn new(
        name: *const c_char,
        queries: *const ComponentAccessDesc,
        query_count: usize,
        callback: DynamicSystemCallback,
        user_data: *mut c_void,
    ) -> Self {
        Self {
            struct_size: core::mem::size_of::<Self>() as u32,
            name,
            queries,
            query_count,
            callback,
            user_data,
        }
    }

    /// Promotes this descriptor into a [`ScheduledSystemDescriptor`] targeting `schedule`.
    #[inline]
    pub const fn with_schedule(self, schedule: ScheduleLabelId) -> ScheduledSystemDescriptor {
        ScheduledSystemDescriptor::from_base(self, schedule)
    }

    /// Promotes this descriptor into a [`ConditionalSystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalSystemDescriptor {
        ConditionalSystemDescriptor::from_scheduled(
            ScheduledSystemDescriptor::from_base(self, ScheduleLabelId::UPDATE),
            Some(run_if),
            run_if_user_data,
        )
    }

    /// Validates `struct_size` and mandatory pointer invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if self.name.is_null() || (self.query_count > 0 && self.queries.is_null()) {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `SystemDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and
// `validate_raw_slot` verifies the non-nullable `callback` pointer before `assume_init()`.
unsafe impl ExtensibleStruct for SystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let cb_offset = core::mem::offset_of!(Self, callback);
        // SAFETY: `cb_offset + size_of::<usize>() <= size_of::<Self>()` inside `slot_ptr`.
        let cb_addr = unsafe {
            core::ptr::read_unaligned((slot_ptr as *const u8).add(cb_offset) as *const usize)
        };
        if cb_addr == 0 {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

/// Extended system registration descriptor adding Bevy schedule selection and intra-mod ordering.
///
/// Layout-prefixed by [`SystemDescriptor`] at offset `0` (`88` bytes on 64-bit, `48` bytes on 32-bit).
/// When an older v0.1 [`SystemDescriptor`] (`48` bytes) is read via
/// `read_descriptor::<ScheduledSystemDescriptor>`, the trailing fields are zero-filled to
/// `schedule = ScheduleLabelId::UPDATE` (`0`), `flags = 0`, and empty `after`/`before` slices.
/// Newer guests can pass [`Self::as_base_ptr`] directly to `HostApi::register_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ScheduledSystemDescriptor {
    /// Base v0.1 system descriptor header (`base.struct_size` set to `size_of::<Self>() as u32`).
    pub base: SystemDescriptor,
    /// Target Bevy schedule (`ScheduleLabelId::UPDATE` = `0` when zero-extended).
    pub schedule: ScheduleLabelId,
    /// Reserved flags word (`0` by default).
    pub flags: u32,
    /// Optional array of NUL-terminated system names this system must run after.
    pub after: *const *const c_char,
    /// Number of entries in `after`.
    pub after_count: usize,
    /// Optional array of NUL-terminated system names this system must run before.
    pub before: *const *const c_char,
    /// Number of entries in `before`.
    pub before_count: usize,
}

/// Alias for [`ScheduledSystemDescriptor`].
pub type SystemDescriptorV2 = ScheduledSystemDescriptor;

impl ScheduledSystemDescriptor {
    /// Minimum valid descriptor size (`SystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = SystemDescriptor::MIN_V1_SIZE;

    /// Wraps a base [`SystemDescriptor`] with a target [`ScheduleLabelId`].
    #[inline]
    pub const fn from_base(mut base: SystemDescriptor, schedule: ScheduleLabelId) -> Self {
        base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            base,
            schedule,
            flags: 0,
            after: core::ptr::null(),
            after_count: 0,
            before: core::ptr::null(),
            before_count: 0,
        }
    }

    /// Declares NUL-terminated system names that this system must run after.
    #[inline]
    pub const fn with_after(mut self, after: *const *const c_char, after_count: usize) -> Self {
        self.after = after;
        self.after_count = after_count;
        self
    }

    /// Declares NUL-terminated system names that this system must run before.
    #[inline]
    pub const fn with_before(mut self, before: *const *const c_char, before_count: usize) -> Self {
        self.before = before;
        self.before_count = before_count;
        self
    }

    /// Promotes this scheduled descriptor into a [`ConditionalSystemDescriptor`] with a `run_if` predicate.
    #[inline]
    pub const fn with_run_if(
        self,
        run_if: RunConditionCallback,
        run_if_user_data: *mut c_void,
    ) -> ConditionalSystemDescriptor {
        ConditionalSystemDescriptor::from_scheduled(self, Some(run_if), run_if_user_data)
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterSystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const SystemDescriptor {
        &self.base
    }

    /// Validates base descriptor fields, schedule label validity, and ordering array pointers.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        let base_res = self.base.validate();
        if base_res.is_err() {
            return base_res;
        }
        if !self.schedule.is_valid() {
            return FfiResult::INVALID_VERSION;
        }
        if (self.after_count > 0 && self.after.is_null())
            || (self.before_count > 0 && self.before.is_null())
        {
            return FfiResult::NULL_POINTER;
        }
        FfiResult::OK
    }
}

// SAFETY: `ScheduledSystemDescriptor` starts with `base: SystemDescriptor` (`struct_size` at offset 0),
// all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`) are valid when zero-filled,
// and `validate_raw_slot` checks `base.callback != 0`.
unsafe impl ExtensibleStruct for ScheduledSystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `base` is at byte offset 0 of `Self`.
        unsafe { SystemDescriptor::validate_raw_slot(slot_ptr as *const SystemDescriptor) }
    }
}

/// Extended v3 system registration descriptor adding an optional `run_if` condition callback.
///
/// Layout-prefixed by [`ScheduledSystemDescriptor`] at offset `0` (`104` bytes on 64-bit,
/// `56` bytes on 32-bit). When an older v1 [`SystemDescriptor`] (`48`B) or v2
/// [`ScheduledSystemDescriptor`] (`88`B) is read via `read_descriptor::<ConditionalSystemDescriptor>`,
/// trailing fields are zero-filled (`run_if = None`, `run_if_user_data = null`).
/// Newer guests can pass [`Self::as_base_ptr`] directly to `HostApi::register_system`,
/// [`Self::as_scheduled_ptr`] to `register_scheduled_system`, or `&self` to `register_conditional_system`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct ConditionalSystemDescriptor {
    /// Scheduled v2 system descriptor prefix (`sched.base.struct_size` = `size_of::<Self>() as u32`).
    pub sched: ScheduledSystemDescriptor,
    /// Optional run-condition predicate (`None` / `NULL` means execute unconditionally).
    pub run_if: Option<RunConditionCallback>,
    /// Opaque guest-owned context pointer forwarded to `run_if`.
    pub run_if_user_data: *mut c_void,
}

/// Alias for [`ConditionalSystemDescriptor`].
pub type SystemDescriptorV3 = ConditionalSystemDescriptor;

impl ConditionalSystemDescriptor {
    /// Minimum valid descriptor size (`SystemDescriptor::MIN_V1_SIZE` for backward compatibility).
    pub const MIN_V1_SIZE: usize = SystemDescriptor::MIN_V1_SIZE;

    /// Wraps a [`ScheduledSystemDescriptor`] with an optional `run_if` predicate.
    #[inline]
    pub const fn from_scheduled(
        mut sched: ScheduledSystemDescriptor,
        run_if: Option<RunConditionCallback>,
        run_if_user_data: *mut c_void,
    ) -> Self {
        sched.base.struct_size = core::mem::size_of::<Self>() as u32;
        Self {
            sched,
            run_if,
            run_if_user_data,
        }
    }

    /// Returns a base-compatible pointer suitable for passing to [`RegisterSystemFn`].
    #[inline]
    pub const fn as_base_ptr(&self) -> *const SystemDescriptor {
        &self.sched.base
    }

    /// Returns a v2-compatible pointer suitable for passing to [`RegisterScheduledSystemFn`].
    #[inline]
    pub const fn as_scheduled_ptr(&self) -> *const ScheduledSystemDescriptor {
        &self.sched
    }

    /// Validates base, schedule, and ordering invariants.
    #[inline]
    pub fn validate(&self) -> FfiResult {
        self.sched.validate()
    }
}

// SAFETY: `ConditionalSystemDescriptor` starts with `sched: ScheduledSystemDescriptor`
// (`struct_size` at offset 0), all fields after `MIN_V1_SIZE` (`schedule`, `flags`, `after*`, `before*`,
// `run_if: Option<...>`, `run_if_user_data`) are valid when zero-filled, and `validate_raw_slot` checks `callback != 0`.
unsafe impl ExtensibleStruct for ConditionalSystemDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        // SAFETY: `sched.base` is at byte offset 0 of `Self`.
        unsafe { SystemDescriptor::validate_raw_slot(slot_ptr as *const SystemDescriptor) }
    }
}

/// System registration function signature exposed in [`HostApi`].
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const SystemDescriptor,
) -> FfiResult;

/// Explicit scheduled system registration function signature resolvable via `GetProcAddressFn`.
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterScheduledSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ScheduledSystemDescriptor,
) -> FfiResult;

/// Explicit conditional system registration function signature resolvable via `GetProcAddressFn`.
///
/// # Safety
/// `host_context` must point to host initialization context; `descriptor` must point to a valid descriptor.
pub type RegisterConditionalSystemFn = unsafe extern "C" fn(
    host_context: *mut c_void,
    descriptor: *const ConditionalSystemDescriptor,
) -> FfiResult;

/// Symbol-resolver function pointer passed to the guest mod entry point (Godot GDExtension style).
///
/// # Safety
/// `name` must be a valid nul-terminated C string or null.
pub type GetProcAddressFn = unsafe extern "C" fn(name: *const c_char) -> *const c_void;

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
/// use abi_contract::{ModComponentDescriptor, ComponentUuid, FfiResult};
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
    /// zero-filled via [`read_extensible_struct`].
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

// SAFETY: `ModComponentDescriptor` is `#[repr(C)]` with `struct_size: u32` at offset 0 and all
// fields (`drop_fn: Option<...>`, `vtable_ptr: *const c_void`, etc.) are valid when zero-filled.
unsafe impl ExtensibleStruct for ModComponentDescriptor {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;
}

/// Dynamic component registration function signature exposed in [`HostApi`] and via `get_proc_address`.
///
/// # Safety
/// - `host_context` must point to valid host engine context provided during mod initialization.
/// - `descriptor` must point to a valid, initialized [`ModComponentDescriptor`].
pub type RegisterComponentFn = unsafe extern "C" fn(
    host_context: *mut core::ffi::c_void,
    descriptor: *const ModComponentDescriptor,
) -> FfiResult;

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
    /// Register a runtime dynamic component with the host Bevy ECS World.
    pub register_component: RegisterComponentFn,
}

impl HostApi {
    /// Size of the legacy 7-function `HostApi` prior to `register_component` (`64` on 64-bit, `36` on 32-bit).
    pub const LEGACY_64_SIZE: usize = core::mem::offset_of!(Self, register_component);
    /// Minimum valid full v0.1 `HostApi` size (`72` on 64-bit, `40` on 32-bit).
    pub const MIN_V1_SIZE: usize = core::mem::size_of::<Self>();

    /// Safely reads the `struct_size` header from a potentially smaller/older `HostApi` pointer
    /// without forming a full `&HostApi` reference (preventing out-of-bounds provenance UB).
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least `size_of::<u32>()` bytes.
    #[inline]
    pub unsafe fn raw_struct_size(api_ptr: *const Self) -> Option<u32> {
        if api_ptr.is_null() {
            return None;
        }
        // SAFETY: Caller guarantees at least 4 readable, aligned bytes at offset 0 (`struct_size`).
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).struct_size)) })
    }

    /// Safely reads the `abi_version` header from a potentially smaller/older `HostApi` pointer
    /// without forming a full `&HostApi` reference.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least 8 bytes.
    #[inline]
    pub unsafe fn raw_abi_version(api_ptr: *const Self) -> Option<u32> {
        let sz = unsafe { Self::raw_struct_size(api_ptr) }?;
        if (sz as usize) < core::mem::offset_of!(Self, register_system) {
            return None;
        }
        // SAFETY: Verified `struct_size >= 8`, so `abi_version` at offset 4 is initialized and readable.
        Some(unsafe { core::ptr::read(core::ptr::addr_of!((*api_ptr).abi_version)) })
    }

    /// Returns `true` if `api_ptr` is non-null and its `struct_size` covers `register_component`.
    ///
    /// # Safety
    /// If `api_ptr` is non-null, it must be aligned and readable for at least `size_of::<u32>()` bytes.
    #[inline]
    pub unsafe fn has_register_component_raw(api_ptr: *const Self) -> bool {
        match unsafe { Self::raw_struct_size(api_ptr) } {
            Some(sz) => (sz as usize) >= Self::MIN_V1_SIZE,
            None => false,
        }
    }

    /// Validates `struct_size` and semver compatibility against a guest's `guest_packed_version`.
    #[inline]
    pub fn validate_for_guest(&self, guest_packed_version: u32) -> FfiResult {
        if (self.struct_size as usize) < Self::MIN_V1_SIZE {
            return FfiResult::INVALID_VERSION;
        }
        if !abi_version_is_compatible(self.abi_version, guest_packed_version) {
            return FfiResult::INVALID_VERSION;
        }
        FfiResult::OK
    }
}

// SAFETY: `HostApi` is `#[repr(C)]` with `struct_size: u32` at offset 0 and `validate_raw_slot`
// verifies all 8 non-nullable function pointers before `assume_init()`.
unsafe impl ExtensibleStruct for HostApi {
    const MIN_V1_SIZE: usize = Self::MIN_V1_SIZE;

    #[inline]
    unsafe fn validate_raw_slot(slot_ptr: *const Self) -> FfiResult {
        let fn_offsets = [
            core::mem::offset_of!(Self, register_system),
            core::mem::offset_of!(Self, iter_next_chunk),
            core::mem::offset_of!(Self, iter_column_readonly),
            core::mem::offset_of!(Self, iter_column_mutable),
            core::mem::offset_of!(Self, iter_mark_changed),
            core::mem::offset_of!(Self, iter_entities),
            core::mem::offset_of!(Self, iter_column_stride),
            core::mem::offset_of!(Self, register_component),
        ];
        let mut i = 0;
        while i < fn_offsets.len() {
            // SAFETY: Every offset in `fn_offsets` lies strictly within `0..size_of::<Self>()`.
            let addr = unsafe {
                core::ptr::read_unaligned((slot_ptr as *const u8).add(fn_offsets[i]) as *const usize)
            };
            if addr == 0 {
                return FfiResult::NULL_POINTER;
            }
            i += 1;
        }
        FfiResult::OK
    }
}

/// Mod initialization entry point function signature (`bevy_abi_mod_init`).
///
/// # Safety
/// Invoked dynamically by host during `load_mod`. Pointers must remain valid for the duration of the handshake.
pub type ModInitFn = unsafe extern "C" fn(
    get_proc_address: GetProcAddressFn,
    host_api: *const HostApi,
    host_context: *mut c_void,
) -> FfiResult;

/// Mod teardown function signature (`bevy_abi_mod_shutdown`).
///
/// # Safety
/// Invoked dynamically by host during `unload_mod` while library memory is still mapped.
pub type ModShutdownFn = unsafe extern "C" fn(host_context: *mut c_void) -> FfiResult;

/// Optional ABI version query entry point (`bevy_abi_mod_abi_version`).
///
/// # Safety
/// Must be side-effect free and return packed ABI version.
pub type ModAbiVersionFn = unsafe extern "C" fn() -> u32;

/// Marker trait for types without internal heap ownership, drop glue, or invalid bit patterns
/// that are safe to expose via interior pointers (`<Struct>_ptr_<field>`) or raw byte reads.
///
/// # Safety
/// Implementing this trait asserts that `Self`:
/// - Contains **no** heap-allocated collections (`String`, `Vec`, `Box`, etc.) or non-`'static` borrows,
/// - Requires **no** drop glue (`!core::mem::needs_drop::<Self>()`), and
/// - Has **no** invalid bit-pattern hazards (`bool`, `char`, `enum`, `NonZero*`, references, or `fn` pointers
///   do **not** implement `FfiLayout`).
///
/// Note: `FfiLayout` does **not** require `#[repr(C)]` field ordering. For cross-language C-ABI
/// column slicing, `Vec<T>` element slices, and mirrored guest structs, require [`AbiPod`].
pub unsafe trait FfiLayout: Sized + 'static {}

/// Marker trait for Plain Old Data (POD) types with a guaranteed `#[repr(C)]` (or primitive /
/// `#[repr(transparent)]`) memory representation, `Copy` semantics, no heap pointers or drop glue,
/// and all bit patterns valid across FFI.
///
/// # Safety
/// Implementing `AbiPod` asserts all invariants of [`FfiLayout`] **plus**:
/// - `Self: Copy + 'static`,
/// - `Self` is a C-compatible primitive, `[T; N]` where `T: AbiPod`, or a `#[repr(C)]` /
///   `#[repr(transparent)]` struct whose fields all implement `AbiPod` (with no skipped fields),
/// - `bool` is **not** `AbiPod` because byte values `2..=255` are undefined behavior in Rust.
pub unsafe trait AbiPod: FfiLayout + Copy + 'static {}

macro_rules! impl_abi_pod_primitives {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Numeric/opaque-integer primitive with `Copy`, `'static`, zero drop glue,
            // zero heap ownership, deterministic C ABI representation, and all bit patterns valid.
            unsafe impl FfiLayout for $ty {}
            // SAFETY: Every bit pattern is valid and representation matches C primitive layout.
            unsafe impl AbiPod for $ty {}
        )*
    };
}

impl_abi_pod_primitives!(
    u8, u16, u32, u64, u128, usize, i8, i16, i32, i64, i128, isize, f32, f64, FfiEntity
);

// SAFETY: Fixed-size arrays `[T; N]` have contiguous in-line storage with no extra metadata or drop glue.
unsafe impl<T: FfiLayout, const N: usize> FfiLayout for [T; N] {}
// SAFETY: `[T; N]` has identical layout to C's `T[N]` when `T: AbiPod`.
unsafe impl<T: AbiPod, const N: usize> AbiPod for [T; N] {}

/// Host-side symbol table entry mapping a symbol name to its static VTable or function pointer.
///
/// Intentionally a Rust metadata struct (not `#[repr(C)]`) used by `ExposableComponent::symbols()`
/// so the host's `ModRegistry` can populate its `get_proc_address` lookup table.
///
/// # Example
///
/// ```
/// use abi_contract::SymbolEntry;
///
/// let entry = SymbolEntry {
///     name: "Position_vtable",
///     ptr: core::ptr::null(),
/// };
/// assert_eq!(entry.name, "Position_vtable");
/// ```
#[derive(Debug, Copy, Clone)]
pub struct SymbolEntry {
    /// Symbol name exposed to `get_proc_address`.
    pub name: &'static str,
    /// Function or static VTable pointer.
    pub ptr: *const c_void,
}

// SAFETY: `SymbolEntry` only holds a `'static` string slice and an immutable static VTable/fn pointer.
unsafe impl Sync for SymbolEntry {}
// SAFETY: `SymbolEntry` only holds immutable static pointers safe to share across threads.
unsafe impl Send for SymbolEntry {}

/// Trait implemented by `#[derive(ModExposable)]` on host components.
///
/// Exposes the component's string name, deterministic UUID, layout (`size` and `align`),
/// POD/raw-insertion safety capability flags (`IS_POD`, `IS_RAW_INSERTABLE`),
/// static `#[repr(C)]` VTable, and C-ABI accessor symbol table for `ModRegistry` registration.
///
/// Decoupled from [`FfiLayout`] and [`AbiPod`] so components containing `String`, `Vec<T>`, `bool`,
/// or `#[mod_field(skip)]` fields can safely expose VTable accessors without falsely claiming raw
/// byte-copy soundness.
pub trait ExposableComponent: Sized + 'static {
    /// The string name of the component (e.g., `"Position"` or `"game::Position"`).
    const COMPONENT_NAME: &'static str;

    /// Null-terminated C string literal of the component name.
    const COMPONENT_CSTR: &'static core::ffi::CStr;

    /// Deterministic 128-bit UUID for this component.
    const UUID: ComponentUuid;

    /// `true` if and only if `Self` is `#[repr(C)]` / `#[repr(transparent)]`, has no skipped fields,
    /// and every field is [`AbiPod`] (safe for cross-language column slicing and raw byte insertion).
    const IS_POD: bool = false;

    /// `true` if and only if `Self` contains no heap pointers (`String`, `Vec`), no drop glue,
    /// no invalid bit-pattern types (`bool`), and no skipped fields, making `ptr::read_unaligned`
    /// from foreign command-queue bytes free of allocator-crossover and invalid-value UB.
    const IS_RAW_INSERTABLE: bool = false;

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

    #[test]
    fn test_mod_component_descriptor_layout_and_offsets() {
        assert_eq!(core::mem::size_of::<ModComponentDescriptor>(), 64);
        assert_eq!(core::mem::align_of::<ModComponentDescriptor>(), 8);

        assert_eq!(
            core::mem::offset_of!(ModComponentDescriptor, struct_size),
            0
        );
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, name), 8);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, uuid), 16);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, size), 32);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, align), 40);
        assert_eq!(core::mem::offset_of!(ModComponentDescriptor, drop_fn), 48);
        assert_eq!(
            core::mem::offset_of!(ModComponentDescriptor, vtable_ptr),
            56
        );
    }

    #[test]
    fn test_host_api_layout_and_offsets() {
        assert_eq!(core::mem::size_of::<HostApi>(), 72);
        assert_eq!(core::mem::align_of::<HostApi>(), 8);

        assert_eq!(core::mem::offset_of!(HostApi, struct_size), 0);
        assert_eq!(core::mem::offset_of!(HostApi, abi_version), 4);
        assert_eq!(core::mem::offset_of!(HostApi, register_system), 8);
        assert_eq!(core::mem::offset_of!(HostApi, iter_next_chunk), 16);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_readonly), 24);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_mutable), 32);
        assert_eq!(core::mem::offset_of!(HostApi, iter_mark_changed), 40);
        assert_eq!(core::mem::offset_of!(HostApi, iter_entities), 48);
        assert_eq!(core::mem::offset_of!(HostApi, iter_column_stride), 56);
        assert_eq!(core::mem::offset_of!(HostApi, register_component), 64);
    }

    #[test]
    fn test_mod_component_descriptor_validation_matrix() {
        let name = c"CustomData".as_ptr();
        let uuid = ComponentUuid::from_name("CustomData");

        // 1. Valid standard POD component
        let valid_desc = ModComponentDescriptor::new(name, uuid, 16, 8);
        assert!(valid_desc.is_valid());
        assert_eq!(valid_desc.validate(), FfiResult::OK);
        assert!(!valid_desc.is_zst());
        assert!(valid_desc.is_pod());
        assert!(valid_desc.layout().is_some());
        unsafe {
            assert_eq!(
                valid_desc.name_cstr().unwrap().to_str().unwrap(),
                "CustomData"
            );
        }

        // 2. Valid Zero-Sized Type (ZST)
        let zst_desc = ModComponentDescriptor::new(name, uuid, 0, 1);
        assert!(zst_desc.is_valid());
        assert_eq!(zst_desc.validate(), FfiResult::OK);
        assert!(zst_desc.is_zst());
        assert!(zst_desc.is_pod());
        assert_eq!(zst_desc.layout().unwrap().size(), 0);
        assert_eq!(zst_desc.layout().unwrap().align(), 1);

        // 3. Valid over-aligned ZST
        let overaligned_zst = ModComponentDescriptor::new(name, uuid, 0, 16);
        assert!(overaligned_zst.is_valid());
        assert_eq!(overaligned_zst.validate(), FfiResult::OK);
        assert_eq!(overaligned_zst.layout().unwrap().align(), 16);

        // 4. Invalid null name
        let null_name = ModComponentDescriptor::new(core::ptr::null(), uuid, 16, 8);
        assert!(!null_name.is_valid());
        assert_eq!(null_name.validate(), FfiResult::NULL_POINTER);
        unsafe {
            assert!(null_name.name_cstr().is_none());
        }

        // 5. Invalid zero alignment
        let mut zero_align = valid_desc;
        zero_align.align = 0;
        assert!(!zero_align.is_valid());
        assert_eq!(zero_align.validate(), FfiResult::INVALID_COMPONENT);
        assert!(zero_align.layout().is_none());

        // 6. Invalid non-power-of-two alignment
        let mut non_pot_align = valid_desc;
        non_pot_align.align = 7;
        assert!(!non_pot_align.is_valid());
        assert_eq!(non_pot_align.validate(), FfiResult::INVALID_COMPONENT);
        assert!(non_pot_align.layout().is_none());

        // 7. Invalid misaligned size (size % align != 0)
        let mut misaligned = valid_desc;
        misaligned.size = 10;
        misaligned.align = 4;
        assert!(!misaligned.is_valid());
        assert_eq!(misaligned.validate(), FfiResult::INVALID_COMPONENT);
        assert!(misaligned.layout().is_none());

        // 8. Invalid truncated struct_size (< 64)
        let mut truncated = valid_desc;
        truncated.struct_size = 40;
        assert!(!truncated.is_valid());
        assert_eq!(truncated.validate(), FfiResult::INVALID_VERSION);

        // 9. Valid forward-compatible struct_size (>= 64)
        let mut forward_compat = valid_desc;
        forward_compat.struct_size = 128;
        assert!(forward_compat.is_valid());
        assert_eq!(forward_compat.validate(), FfiResult::OK);

        // 10. with_drop and with_vtable
        unsafe extern "C" fn dummy_drop(_ptr: *mut core::ffi::c_void) {}
        let with_drop = valid_desc.with_drop(dummy_drop);
        assert!(!with_drop.is_pod());
        assert_eq!(
            with_drop.drop_fn.map(|f| f as *const () as usize),
            Some(dummy_drop as *const () as usize)
        );

        let vtable_dummy = 0x1234usize as *const core::ffi::c_void;
        let with_vtable = valid_desc.with_vtable(vtable_dummy);
        assert_eq!(with_vtable.vtable_ptr, vtable_dummy);
    }

    #[test]
    fn test_ffi_entity_null_sentinel() {
        assert_eq!(FfiEntity::NULL.to_bits(), 0);
        assert!(FfiEntity::NULL.is_null());
        assert_eq!(FfiEntity::default(), FfiEntity::NULL);

        let valid_first_entity = FfiEntity::from_bits(0xFFFF_FFFE);
        assert!(!valid_first_entity.is_null());
        let bevy_placeholder_bits = FfiEntity::from_bits(1);
        assert!(!bevy_placeholder_bits.is_null());
    }

    #[test]
    fn test_abi_version_semver_compatibility_matrix() {
        let host_0_1_0 = pack_abi_version(0, 1, 0);
        let host_0_3_2 = pack_abi_version(0, 3, 2);
        let host_1_0_0 = pack_abi_version(1, 0, 0);

        // Exact match and patch differences
        assert!(abi_version_is_compatible(
            host_0_1_0,
            pack_abi_version(0, 1, 0)
        ));
        assert!(abi_version_is_compatible(
            host_0_1_0,
            pack_abi_version(0, 1, 9)
        ));
        assert!(abi_version_is_compatible(
            pack_abi_version(0, 1, 5),
            host_0_1_0
        ));

        // Older minor guest on newer minor host -> compatible
        assert!(abi_version_is_compatible(
            host_0_3_2,
            pack_abi_version(0, 1, 0)
        ));
        assert!(abi_version_is_compatible(
            host_0_3_2,
            pack_abi_version(0, 2, 7)
        ));
        assert!(abi_version_is_compatible(
            host_0_3_2,
            pack_abi_version(0, 3, 99)
        ));

        // Newer minor guest on older minor host -> incompatible
        assert!(!abi_version_is_compatible(
            host_0_1_0,
            pack_abi_version(0, 2, 0)
        ));
        assert!(!abi_version_is_compatible(
            host_0_3_2,
            pack_abi_version(0, 4, 0)
        ));

        // Different major version -> always incompatible
        assert!(!abi_version_is_compatible(host_1_0_0, host_0_1_0));
        assert!(!abi_version_is_compatible(host_0_3_2, host_1_0_0));
    }

    #[test]
    fn test_schedule_label_id_variants_and_default() {
        assert_eq!(ScheduleLabelId::default(), ScheduleLabelId::UPDATE);
        assert_eq!(ScheduleLabelId::UPDATE.0, 0);
        assert_eq!(ScheduleLabelId::STARTUP.0, 1);
        assert_eq!(ScheduleLabelId::PRE_UPDATE.0, 2);
        assert_eq!(ScheduleLabelId::POST_UPDATE.0, 3);
        assert_eq!(ScheduleLabelId::FIXED_UPDATE.0, 4);
        assert_eq!(ScheduleLabelId::LAST.0, 5);

        for raw in 0..=5 {
            assert!(ScheduleLabelId(raw).is_valid());
        }
        assert!(!ScheduleLabelId(6).is_valid());
        assert!(!ScheduleLabelId(u32::MAX).is_valid());
    }

    #[test]
    fn test_scheduled_system_descriptor_layout_and_zero_extension() {
        unsafe extern "C" fn dummy_sys(
            _iter: *mut RawChunkIter,
            _cmds: *mut RawCommandQueue,
            _ud: *mut c_void,
        ) -> FfiResult {
            FfiResult::OK
        }

        #[cfg(target_pointer_width = "64")]
        {
            assert_eq!(core::mem::size_of::<SystemDescriptor>(), 48);
            assert_eq!(core::mem::size_of::<ScheduledSystemDescriptor>(), 88);
            assert_eq!(core::mem::align_of::<ScheduledSystemDescriptor>(), 8);
            assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, base), 0);
            assert_eq!(
                core::mem::offset_of!(ScheduledSystemDescriptor, schedule),
                48
            );
            assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, flags), 52);
            assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, after), 56);
            assert_eq!(
                core::mem::offset_of!(ScheduledSystemDescriptor, after_count),
                64
            );
            assert_eq!(core::mem::offset_of!(ScheduledSystemDescriptor, before), 72);
            assert_eq!(
                core::mem::offset_of!(ScheduledSystemDescriptor, before_count),
                80
            );
        }

        // 1. Older v0.1 SystemDescriptor (48 bytes) zero-extended to ScheduledSystemDescriptor (88 bytes)
        let v1_desc = SystemDescriptor::new(
            c"legacy_sys".as_ptr(),
            core::ptr::null(),
            0,
            dummy_sys,
            0xBEEFusize as *mut c_void,
        );
        assert_eq!(v1_desc.validate(), FfiResult::OK);

        // SAFETY: `v1_desc` is a valid initialized `SystemDescriptor` on the stack.
        let promoted = unsafe {
            read_descriptor::<ScheduledSystemDescriptor>(&v1_desc as *const _ as *const c_void)
        }
        .expect("v0.1 SystemDescriptor must zero-extend cleanly");

        assert_eq!(
            promoted.base.struct_size,
            core::mem::size_of::<SystemDescriptor>() as u32
        );
        assert_eq!(promoted.schedule, ScheduleLabelId::UPDATE);
        assert_eq!(promoted.flags, 0);
        assert!(promoted.after.is_null());
        assert_eq!(promoted.after_count, 0);
        assert!(promoted.before.is_null());
        assert_eq!(promoted.before_count, 0);
        assert_eq!(promoted.base.user_data as usize, 0xBEEF);
        assert_eq!(promoted.validate(), FfiResult::OK);

        // 2. Full ScheduledSystemDescriptor with Startup schedule and ordering arrays
        let after_names = [c"init_world".as_ptr()];
        let v2_desc = v1_desc
            .with_schedule(ScheduleLabelId::STARTUP)
            .with_after(after_names.as_ptr(), after_names.len());
        assert_eq!(v2_desc.validate(), FfiResult::OK);

        // Passing `as_base_ptr()` through `RegisterSystemFn`-compatible pointer round-trips all v2 fields
        // SAFETY: `v2_desc` is valid for all `size_of::<ScheduledSystemDescriptor>()` bytes.
        let roundtrip = unsafe {
            read_descriptor::<ScheduledSystemDescriptor>(v2_desc.as_base_ptr() as *const c_void)
        }
        .expect("v2 descriptor must read cleanly");
        assert_eq!(roundtrip.schedule, ScheduleLabelId::STARTUP);
        assert_eq!(roundtrip.after_count, 1);
        assert_eq!(roundtrip.after, after_names.as_ptr());

        // 3. Forward-compatible future descriptor (larger than ScheduledSystemDescriptor)
        #[repr(C)]
        #[derive(Copy, Clone)]
        struct FutureSystemDescriptor {
            v2: ScheduledSystemDescriptor,
            future_extra: [u64; 4],
        }
        let mut future_desc = FutureSystemDescriptor {
            v2: v2_desc,
            future_extra: [0xDEAD_BEEF; 4],
        };
        future_desc.v2.base.struct_size = core::mem::size_of::<FutureSystemDescriptor>() as u32;

        // SAFETY: `future_desc` is readable for `size_of::<FutureSystemDescriptor>()` bytes.
        let truncated_future = unsafe {
            read_descriptor::<ScheduledSystemDescriptor>(&future_desc as *const _ as *const c_void)
        }
        .expect("future descriptor with larger struct_size must be accepted");
        assert_eq!(truncated_future.schedule, ScheduleLabelId::STARTUP);
        assert_eq!(truncated_future.after_count, 1);

        // 4. Truncated descriptor (< MIN_V1_SIZE) rejected
        let mut too_small = v1_desc;
        too_small.struct_size = 16;
        // SAFETY: `too_small` is readable for at least 4 bytes (`struct_size`).
        let err = unsafe {
            read_descriptor::<ScheduledSystemDescriptor>(&too_small as *const _ as *const c_void)
        }
        .unwrap_err();
        assert_eq!(err, FfiResult::INVALID_VERSION);

        // 5. Foreign C struct with zeroed/NULL callback pointer rejected BEFORE assume_init() (no niche UB!)
        let mut raw_c_zeroed = [0u8; core::mem::size_of::<SystemDescriptor>()];
        let sz_bytes = (core::mem::size_of::<SystemDescriptor>() as u32).to_ne_bytes();
        raw_c_zeroed[..4].copy_from_slice(&sz_bytes);
        // SAFETY: `raw_c_zeroed` has `size_of::<SystemDescriptor>()` readable bytes with null callback.
        let null_cb_err = unsafe {
            read_descriptor::<ScheduledSystemDescriptor>(raw_c_zeroed.as_ptr() as *const c_void)
        }
        .unwrap_err();
        assert_eq!(null_cb_err, FfiResult::NULL_POINTER);

        // 6. Null descriptor pointer rejected
        // SAFETY: Testing null pointer check.
        assert_eq!(
            unsafe { read_descriptor::<ScheduledSystemDescriptor>(core::ptr::null()) }.unwrap_err(),
            FfiResult::NULL_POINTER
        );
    }

    #[test]
    fn test_host_api_raw_header_inspection_on_legacy_buffer() {
        #[repr(C)]
        struct Legacy64Buffer {
            struct_size: u32,
            abi_version: u32,
            ptrs: [usize; 7],
        }
        let legacy = Legacy64Buffer {
            struct_size: core::mem::size_of::<Legacy64Buffer>() as u32,
            abi_version: ABI_VERSION_PACKED,
            ptrs: [1; 7],
        };
        let api_ptr = &legacy as *const Legacy64Buffer as *const HostApi;
        // SAFETY: `legacy` is aligned and readable for 64 bytes (`>= 8` bytes for header fields).
        unsafe {
            assert_eq!(
                HostApi::raw_struct_size(api_ptr),
                Some(core::mem::size_of::<Legacy64Buffer>() as u32)
            );
            assert_eq!(HostApi::raw_abi_version(api_ptr), Some(ABI_VERSION_PACKED));
            assert!(!HostApi::has_register_component_raw(api_ptr));
            assert_eq!(HostApi::raw_struct_size(core::ptr::null()), None);
        }
    }

    #[test]
    fn test_abi_pod_primitives_and_arrays() {
        fn assert_pod<T: AbiPod>() {}
        assert_pod::<u8>();
        assert_pod::<u16>();
        assert_pod::<u32>();
        assert_pod::<u64>();
        assert_pod::<u128>();
        assert_pod::<usize>();
        assert_pod::<i8>();
        assert_pod::<i16>();
        assert_pod::<i32>();
        assert_pod::<i64>();
        assert_pod::<i128>();
        assert_pod::<isize>();
        assert_pod::<f32>();
        assert_pod::<f64>();
        assert_pod::<FfiEntity>();
        assert_pod::<[f32; 4]>();
        assert_pod::<[[u32; 4]; 4]>();
    }

    #[test]
    fn test_phase3_phase4_access_modes_and_conditional_descriptors() {
        // Verify all 10 valid modes (0..=9) and helper predicates
        let all_modes = [
            ComponentAccessMode::READ,
            ComponentAccessMode::WRITE,
            ComponentAccessMode::FILTER_ONLY,
            ComponentAccessMode::WITHOUT,
            ComponentAccessMode::OPTIONAL_READ,
            ComponentAccessMode::OPTIONAL_WRITE,
            ComponentAccessMode::CHANGED,
            ComponentAccessMode::ADDED,
            ComponentAccessMode::RESOURCE_READ,
            ComponentAccessMode::RESOURCE_WRITE,
        ];
        for (idx, mode) in all_modes.iter().copied().enumerate() {
            assert_eq!(mode.0, idx as u32);
            assert!(mode.is_valid());
        }
        assert!(!ComponentAccessMode(10).is_valid());
        assert!(!ComponentAccessMode(u32::MAX).is_valid());

        assert!(ComponentAccessMode::OPTIONAL_READ.is_optional());
        assert!(ComponentAccessMode::OPTIONAL_WRITE.is_optional());
        assert!(!ComponentAccessMode::READ.is_optional());

        assert!(ComponentAccessMode::WRITE.is_mutable());
        assert!(ComponentAccessMode::OPTIONAL_WRITE.is_mutable());
        assert!(ComponentAccessMode::RESOURCE_WRITE.is_mutable());
        assert!(!ComponentAccessMode::OPTIONAL_READ.is_mutable());
        assert!(!ComponentAccessMode::CHANGED.is_mutable());
        assert!(!ComponentAccessMode::ADDED.is_mutable());
        assert!(!ComponentAccessMode::RESOURCE_READ.is_mutable());

        assert!(ComponentAccessMode::CHANGED.is_tick_filter());
        assert!(ComponentAccessMode::ADDED.is_tick_filter());
        assert!(!ComponentAccessMode::FILTER_ONLY.is_tick_filter());

        assert!(ComponentAccessMode::RESOURCE_READ.is_resource());
        assert!(ComponentAccessMode::RESOURCE_WRITE.is_resource());
        assert!(!ComponentAccessMode::READ.is_resource());

        assert_eq!(
            ComponentAccessDesc::optional_read(c"A".as_ptr()).mode,
            ComponentAccessMode::OPTIONAL_READ
        );
        assert_eq!(
            ComponentAccessDesc::optional_write(c"A".as_ptr()).mode,
            ComponentAccessMode::OPTIONAL_WRITE
        );
        assert_eq!(
            ComponentAccessDesc::changed(c"A".as_ptr()).mode,
            ComponentAccessMode::CHANGED
        );
        assert_eq!(
            ComponentAccessDesc::added(c"A".as_ptr()).mode,
            ComponentAccessMode::ADDED
        );
        assert_eq!(
            ComponentAccessDesc::resource_read(c"R".as_ptr()).mode,
            ComponentAccessMode::RESOURCE_READ
        );
        assert_eq!(
            ComponentAccessDesc::resource_write(c"R".as_ptr()).mode,
            ComponentAccessMode::RESOURCE_WRITE
        );

        unsafe extern "C" fn dummy_sys(
            _iter: *mut RawChunkIter,
            _cmds: *mut RawCommandQueue,
            _ud: *mut c_void,
        ) -> FfiResult {
            FfiResult::OK
        }
        unsafe extern "C" fn dummy_cond(_ud: *mut c_void) -> FfiBool {
            FFI_BOOL_TRUE
        }

        if core::mem::size_of::<usize>() == 8 {
            assert_eq!(core::mem::size_of::<SystemDescriptor>(), 48);
            assert_eq!(core::mem::size_of::<ScheduledSystemDescriptor>(), 88);
            assert_eq!(core::mem::size_of::<ConditionalSystemDescriptor>(), 104);
            assert_eq!(
                core::mem::size_of::<ConditionalMultiQuerySystemDescriptor>(),
                104
            );
            assert_eq!(core::mem::size_of::<HostApi>(), 72);
        }

        // v1 (48B) -> v3 (104B) zero-extension
        let v1 = SystemDescriptor::new(
            c"sys".as_ptr(),
            core::ptr::null(),
            0,
            dummy_sys,
            core::ptr::null_mut(),
        );
        // SAFETY: `v1` is valid and readable for `v1.struct_size` bytes.
        let from_v1 = unsafe {
            read_descriptor::<ConditionalSystemDescriptor>(&v1 as *const _ as *const c_void)
        }
        .unwrap();
        assert_eq!(from_v1.sched.schedule, ScheduleLabelId::UPDATE);
        assert!(from_v1.run_if.is_none());
        assert!(from_v1.run_if_user_data.is_null());

        // v2 (88B) -> v3 (104B) zero-extension
        let v2 = v1.with_schedule(ScheduleLabelId::FIXED_UPDATE);
        // SAFETY: `v2` is valid and readable for `v2.base.struct_size` bytes.
        let from_v2 = unsafe {
            read_descriptor::<ConditionalSystemDescriptor>(&v2 as *const _ as *const c_void)
        }
        .unwrap();
        assert_eq!(from_v2.sched.schedule, ScheduleLabelId::FIXED_UPDATE);
        assert!(from_v2.run_if.is_none());
        assert!(from_v2.run_if_user_data.is_null());

        // v3 (104B) round-trip
        let tag = 0x1234usize as *mut c_void;
        let v3 = v2.with_run_if(dummy_cond, tag);
        // SAFETY: `v3` is valid and readable for `v3.sched.base.struct_size` bytes.
        let from_v3 = unsafe {
            read_descriptor::<ConditionalSystemDescriptor>(&v3 as *const _ as *const c_void)
        }
        .unwrap();
        assert_eq!(from_v3.sched.schedule, ScheduleLabelId::FIXED_UPDATE);
        assert!(from_v3.run_if.is_some());
        assert_eq!(from_v3.run_if_user_data, tag);
        assert_eq!(from_v3.validate(), FfiResult::OK);
    }
}
