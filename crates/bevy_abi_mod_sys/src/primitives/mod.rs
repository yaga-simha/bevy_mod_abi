//! Core FFI primitive types, handles, and status codes.

mod asset;
mod bool;
mod entity;
mod option;
mod slice;
mod uuid;

pub use self::{
    asset::{FfiAssetHandle, FfiAssetLoadState},
    bool::FfiBool,
    entity::{
        BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BamComponent, BamSymbol, BamSystem, BamSystemSet,
        FfiChildOf, FfiEntity,
    },
    option::FfiOption,
    slice::{FfiSlice, StrView},
    uuid::{
        ComponentUuid, FNV1A_64_OFFSET_BASIS, FNV1A_64_PRIME, SIPHASH_KEY_0, SIPHASH_KEY_1,
        fnv1a_64_init, fnv1a_64_str, fnv1a_64_update_bytes, fnv1a_64_update_u64, siphash24_128,
    },
};

/// FFI-safe status code returned across the C-ABI boundary (`typedef uint32_t BamResult;`).
///
/// Represented as a `#[repr(transparent)]` `u32` newtype rather than a Rust `enum`
/// so that arbitrary integer values returned by foreign C, C++, or Zig code never trigger
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
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct FfiResult(pub u32);

impl FfiResult {
    /// Operation succeeded without error (`0`).
    pub const OK: Self = Self(0);
    /// Component handle or registration descriptor is invalid (`1`).
    pub const INVALID_COMPONENT: Self = Self(1);
    /// Requested query or resource access declarations conflict with borrow rules (`2`).
    pub const ACCESS_CONFLICT: Self = Self(2);
    /// Guest callback panicked; caught inside the guest runtime without unwinding across FFI (`3`).
    pub const PANIC_CAUGHT: Self = Self(3);
    /// Null pointer passed for a mandatory pointer or non-empty slice argument (`4`).
    pub const NULL_POINTER: Self = Self(4);
    /// String or C string contains invalid UTF-8 bytes (`5`).
    pub const INVALID_UTF8: Self = Self(5);
    /// Unauthorized operation attempted, such as mutating a read-only or immutable column (`6`).
    pub const PERMISSION_DENIED: Self = Self(6);
    /// ABI version or `struct_size` incompatibility between host and guest (`7`).
    pub const INVALID_VERSION: Self = Self(7);
    /// Invalid layout, size, alignment, duplicate component batch, or non-zero reserved field (`8`).
    pub const INVALID_ARGUMENT: Self = Self(8);
    /// Row, term, or query index out of bounds (`9`).
    pub const OUT_OF_RANGE: Self = Self(9);
    /// Requested symbol, component, resource, or custom schedule was not found (`10`).
    pub const NOT_FOUND: Self = Self(10);
    /// Operation invoked in an invalid lifecycle state, such as registration outside `init` (`11`).
    pub const INVALID_STATE: Self = Self(11);
    /// Entity bit pattern is null, `Entity::PLACEHOLDER`, or despawned (`12`).
    pub const INVALID_ENTITY: Self = Self(12);
    /// Fixed capacity limit exceeded (drop pool, query terms, query count, or spawn batch) (`13`).
    pub const CAPACITY_EXCEEDED: Self = Self(13);
    /// Unknown descriptor flag bit or unsupported storage capability requested (`14`).
    pub const UNSUPPORTED: Self = Self(14);
    /// Component UUID, size, or alignment disagrees with an existing registration (`15`).
    pub const TYPE_MISMATCH: Self = Self(15);
    /// Acyclic relationship edge or schedule ordering graph would form a cycle (`16`).
    pub const CYCLE_DETECTED: Self = Self(16);
    /// Genuine internal host or guest invariant fault (`99`).
    pub const INTERNAL_ERROR: Self = Self(99);

    /// Returns `true` if this result indicates success (`Self::OK`).
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
            Self::TYPE_MISMATCH => "TYPE_MISMATCH",
            Self::CYCLE_DETECTED => "CYCLE_DETECTED",
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

const _: () = {
    assert!(core::mem::size_of::<FfiResult>() == 4);
    assert!(core::mem::align_of::<FfiResult>() == 4);
};
