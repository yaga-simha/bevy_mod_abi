//! POD / raw-insertion marker traits and host component VTable metadata traits.

use core::ffi::c_void;

use crate::primitives::{ComponentUuid, FfiEntity};

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
/// use bevy_abi_mod_sys::SymbolEntry;
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
        Self::vtable_ptr().cast::<c_void>()
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
