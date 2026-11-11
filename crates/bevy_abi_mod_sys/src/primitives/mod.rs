//! Core FFI primitive types and status codes.

mod bool;
mod entity;
mod slice;
mod uuid;

pub use self::{
    bool::{FFI_BOOL_FALSE, FFI_BOOL_TRUE, FfiBool, ffi_bool_to_rust, rust_bool_to_ffi},
    entity::FfiEntity,
    slice::{FfiSlice, StrView},
    uuid::ComponentUuid,
};

/// FFI-safe status code returned across the C-ABI boundary.
///
/// Represented as a `#[repr(transparent)]` `u32` newtype rather than a Rust `enum`
/// so that arbitrary integer values returned by foreign C/C++ code can never trigger
/// invalid-discriminant undefined behavior in Rust.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::FfiResult;
///
/// let res = FfiResult::OK;
/// assert!(res.is_ok());
/// assert!(!res.is_err());
/// assert_eq!(res.name(), "OK");
///
/// match res {
///     FfiResult::OK => {},
///     FfiResult::PANIC_CAUGHT => panic!("caught panic"),
///     _ => {},
/// }
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiResult(pub u32);

impl FfiResult {
    /// Operation succeeded without error.
    pub const OK: Self = Self(0);
    /// Component name or UUID was not found or invalid in the host registry.
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
    /// ABI version or descriptor `struct_size` mismatch between host and mod.
    pub const INVALID_VERSION: Self = Self(7);
    /// Invalid layout, size, alignment, or non-zero reserved argument.
    pub const INVALID_ARGUMENT: Self = Self(8);
    /// Row, column, term, or query index out of bounds.
    pub const OUT_OF_RANGE: Self = Self(9);
    /// Requested symbol, component, resource, or schedule was not found.
    pub const NOT_FOUND: Self = Self(10);
    /// Operation invoked in an invalid lifecycle state (e.g. registration outside `init`).
    pub const INVALID_STATE: Self = Self(11);
    /// Entity bit pattern is null, placeholder, or despawned.
    pub const INVALID_ENTITY: Self = Self(12);
    /// Fixed capacity limit exceeded (e.g. drop pool, query terms, or query count).
    pub const CAPACITY_EXCEEDED: Self = Self(13);
    /// Unknown descriptor flag bit or unsupported storage capability requested.
    pub const UNSUPPORTED: Self = Self(14);
    /// Unexpected internal host or guest invariant fault.
    pub const INTERNAL_ERROR: Self = Self(99);

    /// Transitional alias retained for existing host integration tests.
    #[allow(non_upper_case_globals)]
    #[doc(hidden)]
    pub const PanicCaught: Self = Self::PANIC_CAUGHT;

    /// Returns `true` if this result indicates success (`OK`).
    #[inline]
    pub const fn is_ok(self) -> bool {
        self.0 == Self::OK.0
    }

    /// Returns `true` if this result indicates an error (`self != Self::OK`).
    #[inline]
    pub const fn is_err(self) -> bool {
        self.0 != Self::OK.0
    }

    /// Returns the canonical uppercase constant name for recognized status codes,
    /// or `"UNKNOWN"` for unrecognized foreign integer values.
    #[inline]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OK => "OK",
            Self::INVALID_COMPONENT => "INVALID_COMPONENT",
            Self::ACCESS_CONFLICT => "ACCESS_CONFLICT",
            Self::PANIC_CAUGHT => "PANIC_CAUGHT",
            Self::NULL_POINTER => "NULL_POINTER",
            Self::INVALID_UTF8 => "INVALID_UTF8",
            Self::PERMISSION_DENIED => "PERMISSION_DENIED",
            Self::INVALID_VERSION => "INVALID_VERSION",
            Self::INVALID_ARGUMENT => "INVALID_ARGUMENT",
            Self::OUT_OF_RANGE => "OUT_OF_RANGE",
            Self::NOT_FOUND => "NOT_FOUND",
            Self::INVALID_STATE => "INVALID_STATE",
            Self::INVALID_ENTITY => "INVALID_ENTITY",
            Self::CAPACITY_EXCEEDED => "CAPACITY_EXCEEDED",
            Self::UNSUPPORTED => "UNSUPPORTED",
            Self::INTERNAL_ERROR => "INTERNAL_ERROR",
            _ => "UNKNOWN",
        }
    }
}

impl core::fmt::Display for FfiResult {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let label = self.name();
        if self.0 != Self::OK.0 && label.len() == "UNKNOWN".len() && label.as_bytes() == b"UNKNOWN"
        {
            write!(f, "UNKNOWN({})", self.0)
        } else {
            f.write_str(label)
        }
    }
}
