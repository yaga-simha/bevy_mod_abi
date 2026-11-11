//! World-scoped component handles and opaque 64-bit entity identifiers.

/// 1-based world-scoped component or resource registry handle (`typedef uint32_t BamComponent;`).
///
/// `0` ([`BamComponent::INVALID`]) is reserved as the invalid/uninitialized sentinel so
/// zero-initialized static handles fail deterministically with [`crate::FfiResult::INVALID_COMPONENT`].
/// Valid handles start at `1` and map to dense `O(1)` host registry table indices rather than
/// exposing raw Bevy `ComponentId`s across the ABI.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::BamComponent;
///
/// assert!(!BamComponent::INVALID.is_valid());
/// assert_eq!(BamComponent::default(), BamComponent::INVALID);
/// assert!(BamComponent::from_raw(1).is_valid());
/// ```
#[repr(transparent)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Default)]
pub struct BamComponent(pub u32);

impl BamComponent {
    /// Reserved invalid/uninitialized component handle sentinel (`0`, C `BAM_COMPONENT_INVALID`).
    pub const INVALID: Self = Self(0);

    /// Constructs a `BamComponent` handle from a raw 1-based `u32` value.
    #[inline]
    pub const fn from_raw(raw: u32) -> Self {
        Self(raw)
    }

    /// Extracts the underlying `u32` handle value.
    #[inline]
    pub const fn to_raw(self) -> u32 {
        self.0
    }

    /// Returns `true` if this handle is non-zero (`self.0 != 0`).
    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }
}

/// Opaque 64-bit entity identifier matching `bevy_ecs::entity::Entity::to_bits()`
/// (`typedef uint64_t BamEntity;`).
///
/// Foreign code must treat this value as an opaque 64-bit token without assuming index or
/// generation bit layouts. In Bevy 0.20+, `0` ([`FfiEntity::NULL`]) is guaranteed never to be
/// a valid `Entity` (`Entity::try_from_bits(0)` is `None`), whereas `u64::MAX` is a valid entity
/// bit pattern and `Entity::PLACEHOLDER.to_bits()` is `0x1`.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::FfiEntity;
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
    /// Canonical null / invalid entity sentinel (`0`, C `BAM_ENTITY_NULL`).
    pub const NULL: Self = Self(0);

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

const _: () = {
    assert!(core::mem::size_of::<BamComponent>() == 4);
    assert!(core::mem::align_of::<BamComponent>() == 4);
    assert!(core::mem::size_of::<FfiEntity>() == 8);
};
