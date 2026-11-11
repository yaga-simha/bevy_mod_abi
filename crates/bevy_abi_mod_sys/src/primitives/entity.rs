//! Opaque 64-bit entity identifier across the C-ABI boundary.

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
