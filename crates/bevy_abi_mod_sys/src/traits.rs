//! `RawInsertable`, `AbiPod`, and `ModComponent` traits for component layout and VTable metadata.

use core::ffi::c_void;

use crate::{
    BamChangeTicks,
    primitives::{BamComponent, ComponentUuid, FfiBool, FfiChildOf, FfiEntity},
};

/// Marker trait for types without internal heap ownership, drop glue, or invalid bit patterns
/// that support same-process raw byte moves and unaligned reads.
///
/// Platform-sized integers (`usize`, `isize`) and 128-bit integers (`u128`, `i128`) implement
/// `RawInsertable` without implementing [`AbiPod`] because their size or alignment varies across
/// C99/C++/Zig compilers and target architectures.
///
/// # Safety
/// Implementing `RawInsertable` asserts that `Self`:
/// - Contains no heap-owned allocations (`String`, `Vec`, `Box`) or non-`'static` borrows,
/// - Requires no drop glue (`!core::mem::needs_drop::<Self>()`), and
/// - Has no invalid bit-pattern hazards (`bool`, `char`, `enum`, `NonZero*`, references, and bare
///   `fn` pointers must not implement `RawInsertable`).
pub unsafe trait RawInsertable: Sized + Send + Sync + 'static {}

/// Marker trait for cross-language Plain Old Data (POD) types with deterministic C99/C++/Zig/Rust
/// `#[repr(C)]` or `#[repr(transparent)]` layout, `Copy` semantics, no drop glue, and universal
/// bit validity across all fields.
///
/// Natural C inter-field or trailing alignment padding is permitted when every field is `AbiPod`.
///
/// # Excluded Primitives
/// - `u128` and `i128` do not implement `AbiPod` (no ISO C99 equivalent; 8-byte vs 16-byte
///   alignment divergence across compilers and targets):
///
/// ```compile_fail
/// use bevy_abi_mod_sys::assert_abi_pod;
/// assert_abi_pod::<u128>();
/// ```
///
/// ```compile_fail
/// use bevy_abi_mod_sys::assert_abi_pod;
/// assert_abi_pod::<i128>();
/// ```
///
/// - `usize` and `isize` do not implement `AbiPod` (pointer-width dependent):
///
/// ```compile_fail
/// use bevy_abi_mod_sys::assert_abi_pod;
/// assert_abi_pod::<usize>();
/// ```
///
/// ```compile_fail
/// use bevy_abi_mod_sys::assert_abi_pod;
/// assert_abi_pod::<isize>();
/// ```
///
/// - `bool` and `char` do not implement `AbiPod` (invalid bit patterns; use [`FfiBool`] instead):
///
/// ```compile_fail
/// use bevy_abi_mod_sys::assert_abi_pod;
/// assert_abi_pod::<bool>();
/// ```
///
/// # Safety
/// Implementing `AbiPod` asserts all invariants of [`RawInsertable`] plus:
/// - `Self: Copy + Send + Sync + 'static`, and
/// - `Self` is a permitted fixed-width C primitive, `[T; N]` where `T: AbiPod`, or a non-packed
///   `#[repr(C)]` / `#[repr(transparent)]` struct whose fields all implement `AbiPod`.
pub unsafe trait AbiPod: RawInsertable + Copy + Send + Sync + 'static {}

/// Compile-time helper asserting `T: AbiPod` (used by `#[derive(AbiPod)]` per-field checks).
#[inline]
pub const fn assert_abi_pod<T: AbiPod>() {}

/// Compile-time helper asserting `T: RawInsertable`.
#[inline]
pub const fn assert_raw_insertable<T: RawInsertable>() {}

macro_rules! impl_abi_pod_primitives {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Fixed-width primitive or transparent newtype with `Copy`, `Send + Sync`,
            // zero drop glue, deterministic C99 layout, and every bit pattern valid.
            unsafe impl RawInsertable for $ty {}
            // SAFETY: Layout and alignment match fixed-width C99 representation across targets.
            unsafe impl AbiPod for $ty {}
        )*
    };
}

macro_rules! impl_raw_insertable_only {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: Integer primitive with `Copy`, `Send + Sync`, zero drop glue, and all bit
            // patterns valid within the same process, excluded from cross-language `AbiPod`.
            unsafe impl RawInsertable for $ty {}
        )*
    };
}

impl_abi_pod_primitives!(
    u8,
    i8,
    u16,
    i16,
    u32,
    i32,
    u64,
    i64,
    f32,
    f64,
    FfiBool,
    BamComponent,
    FfiEntity,
    FfiChildOf,
    BamChangeTicks,
    ComponentUuid,
);

impl_raw_insertable_only!(u128, i128, usize, isize);

// SAFETY: Fixed-size arrays `[T; N]` have contiguous inline storage with no drop glue or metadata.
unsafe impl<T: RawInsertable, const N: usize> RawInsertable for [T; N] {}
// SAFETY: `[T; N]` matches C's `T[N]` layout identically when `T: AbiPod`.
unsafe impl<T: AbiPod, const N: usize> AbiPod for [T; N] {}

/// Host-side symbol table entry mapping a `"bam.v0.*"` symbol name to its static VTable or fn pointer.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::SymbolEntry;
///
/// let entry = SymbolEntry {
///     name: "bam.v0.my_game::Position.vtable",
///     ptr: core::ptr::null(),
/// };
/// assert_eq!(entry.name, "bam.v0.my_game::Position.vtable");
/// ```
#[derive(Debug, Copy, Clone)]
pub struct SymbolEntry {
    /// Namespaced symbol name exposed to `BamHostApi::get_proc_address`.
    pub name: &'static str,
    /// Static VTable or function pointer.
    pub ptr: *const c_void,
}

// SAFETY: `SymbolEntry` holds an immutable `'static` string slice and a static pointer requiring
// `unsafe` to dereference.
unsafe impl Sync for SymbolEntry {}
// SAFETY: `SymbolEntry` holds immutable static metadata safe to transfer across threads.
unsafe impl Send for SymbolEntry {}

/// Trait implemented by `#[derive(ModComponent)]` on host components and sub-structs.
///
/// Lives in `bevy_abi_mod_sys` without any `bevy_ecs`, [`RawInsertable`], or `Default` supertrait
/// so `#[repr(Rust)]` components containing `String`, `Vec<T>`, `bool`, `#[mod_field(skip)]`, or
/// `#[mod_component(opaque)]` can expose [`crate::VTableHeader`]-prefixed VTables cleanly.
pub trait ModComponent: Sized + 'static {
    /// Canonical namespaced component name (`"<namespace>::<Name>"`, e.g. `"my_game::Position"`).
    const CANONICAL_NAME: &'static str;

    /// NUL-terminated C string literal of [`Self::CANONICAL_NAME`].
    const COMPONENT_CSTR: &'static core::ffi::CStr;

    /// Deterministic 128-bit UUID for this component.
    const UUID: ComponentUuid;

    /// `true` iff `Self` is `#[repr(C)]` / `#[repr(transparent)]`, has no skipped fields, and
    /// every field implements [`AbiPod`].
    const IS_ABI_POD: bool = false;

    /// `true` iff `Self` supports same-process raw byte insertion ([`RawInsertable`]).
    const IS_RAW_INSERTABLE: bool = false;

    /// Component registration flags (`BAM_COMPONENT_FLAG_*`, default `0`).
    const COMPONENT_FLAGS: u32 = 0;

    /// Auto-generated ISO C99 struct, VTable, and layout assertion declarations.
    const C_DECL: &'static str = "";

    /// Auto-generated Zig `extern struct` and VTable declarations.
    const ZIG_DECL: &'static str = "";

    /// Generated `#[repr(C)]` VTable struct prefixed at offset `0` by [`crate::VTableHeader`].
    type VTable: Copy + 'static;

    /// Returns a reference to the static VTable instance for this component.
    fn vtable() -> &'static Self::VTable;

    /// Returns a typed pointer to the static VTable instance for this component.
    #[inline]
    fn vtable_ptr() -> *const Self::VTable {
        Self::vtable() as *const Self::VTable
    }

    /// Returns a type-erased pointer to the static VTable instance.
    #[inline]
    fn erased_vtable_ptr() -> *const c_void {
        Self::vtable_ptr().cast::<c_void>()
    }

    /// Exact byte size (`core::mem::size_of::<Self>()`).
    #[inline]
    fn size() -> usize {
        core::mem::size_of::<Self>()
    }

    /// Exact byte alignment (`core::mem::align_of::<Self>()`).
    #[inline]
    fn align() -> usize {
        core::mem::align_of::<Self>()
    }

    /// Returns all exported `"bam.v0.*"` symbols and VTable entries for this component.
    fn symbols() -> &'static [SymbolEntry];

    /// Looks up a symbol pointer by exact name (for example `"bam.v0.my_game::Position.vtable"`).
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
