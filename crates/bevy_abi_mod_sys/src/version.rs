//! ABI SemVer version packing and compatibility rules.

/// Major version of the ABI contract. Incremented for breaking ABI layout or signature changes
/// once `1.0` is reached.
pub const ABI_VERSION_MAJOR: u32 = 0;
/// Minor version of the ABI contract. Breaking while `ABI_VERSION_MAJOR == 0`; backward-compatible
/// additive capability bumps once `ABI_VERSION_MAJOR >= 1`.
pub const ABI_VERSION_MINOR: u32 = 1;
/// Patch version of the ABI contract. Incremented for wire-identical internal fixes.
pub const ABI_VERSION_PATCH: u32 = 0;

/// Packs `(major, minor, patch)` into a 32-bit ABI version word: `(major << 24) | (minor << 16) | patch`.
///
/// # Panics
/// Panics (or fails at compile time in `const` context) if `major > 0xFF`, `minor > 0xFF`, or
/// `patch > 0xFFFF`, preventing silent bit truncation.
#[inline]
pub const fn pack_abi_version(major: u32, minor: u32, patch: u32) -> u32 {
    assert!(
        major <= 0xFF,
        "ABI major version exceeds 8-bit range (0..=255)"
    );
    assert!(
        minor <= 0xFF,
        "ABI minor version exceeds 8-bit range (0..=255)"
    );
    assert!(
        patch <= 0xFFFF,
        "ABI patch version exceeds 16-bit range (0..=65535)"
    );
    (major << 24) | (minor << 16) | patch
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

/// Returns `true` if a guest mod compiled against `guest_packed` is SemVer-compatible with a
/// host running `host_packed`.
///
/// Follows Cargo SemVer rules:
/// - Pre-1.0 (`major == 0`): minor bumps are breaking, so `host_major == 0`, `guest_major == 0`,
///   and `host_minor == guest_minor` are all required.
/// - Post-1.0 (`major >= 1`): `host_major == guest_major` and `guest_minor <= host_minor`.
/// - Patch differences are compatible in both directions on matching compatible `(major, minor)` pairs.
#[inline]
pub const fn abi_version_is_compatible(host_packed: u32, guest_packed: u32) -> bool {
    let host_major = abi_version_major(host_packed);
    let guest_major = abi_version_major(guest_packed);
    if host_major != guest_major {
        return false;
    }
    let host_minor = abi_version_minor(host_packed);
    let guest_minor = abi_version_minor(guest_packed);
    if host_major == 0 {
        host_minor == guest_minor
    } else {
        guest_minor <= host_minor
    }
}

/// Packed 32-bit ABI version: `(MAJOR << 24) | (MINOR << 16) | PATCH`.
///
/// Bit packing allows host and guest to negotiate compatibility with a single 32-bit integer comparison.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{
///     abi_version_is_compatible, pack_abi_version, ABI_VERSION_MAJOR, ABI_VERSION_MINOR,
///     ABI_VERSION_PACKED, ABI_VERSION_PATCH,
/// };
///
/// assert_eq!((ABI_VERSION_PACKED >> 24) & 0xFF, ABI_VERSION_MAJOR);
/// assert_eq!((ABI_VERSION_PACKED >> 16) & 0xFF, ABI_VERSION_MINOR);
/// assert_eq!(ABI_VERSION_PACKED & 0xFFFF, ABI_VERSION_PATCH);
/// assert!(abi_version_is_compatible(pack_abi_version(0, 1, 5), ABI_VERSION_PACKED));
/// assert!(!abi_version_is_compatible(pack_abi_version(0, 2, 0), ABI_VERSION_PACKED));
/// assert!(abi_version_is_compatible(pack_abi_version(1, 2, 0), pack_abi_version(1, 1, 0)));
/// ```
pub const ABI_VERSION_PACKED: u32 =
    pack_abi_version(ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH);
