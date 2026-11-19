//! ABI SemVer version packing and compatibility rules.

/// Major version of the ABI contract. Incremented for breaking ABI layout or signature changes
/// once `1.0` is reached.
pub const ABI_VERSION_MAJOR: u32 = 0;

/// Minor version of the ABI contract. Breaking while `ABI_VERSION_MAJOR == 0`; backward-compatible
/// additive capability bumps once `ABI_VERSION_MAJOR >= 1`.
pub const ABI_VERSION_MINOR: u32 = 3;

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

/// Unpacks a 32-bit packed ABI version word into `(major, minor, patch)`.
#[inline]
pub const fn unpack_abi_version(version: u32) -> (u32, u32, u32) {
    (
        (version >> 24) & 0xFF,
        (version >> 16) & 0xFF,
        version & 0xFFFF,
    )
}

/// Extracts the 8-bit major version from a packed ABI version word.
#[inline]
pub const fn abi_version_major(version: u32) -> u32 {
    (version >> 24) & 0xFF
}

/// Extracts the 8-bit minor version from a packed ABI version word.
#[inline]
pub const fn abi_version_minor(version: u32) -> u32 {
    (version >> 16) & 0xFF
}

/// Extracts the 16-bit patch version from a packed ABI version word.
#[inline]
pub const fn abi_version_patch(version: u32) -> u32 {
    version & 0xFFFF
}

/// Packed 32-bit ABI version word (`0x0003_0000` for `0.3.0`).
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::{
///     abi_version_is_compatible, pack_abi_version, unpack_abi_version, ABI_VERSION,
///     ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH,
/// };
///
/// assert_eq!(
///     unpack_abi_version(ABI_VERSION),
///     (ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH),
/// );
/// assert!(abi_version_is_compatible(pack_abi_version(0, 3, 5), ABI_VERSION));
/// assert!(!abi_version_is_compatible(pack_abi_version(0, 2, 0), ABI_VERSION));
/// assert!(abi_version_is_compatible(pack_abi_version(1, 2, 0), pack_abi_version(1, 1, 0)));
/// ```
pub const ABI_VERSION: u32 =
    pack_abi_version(ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH);

/// Returns `true` if a guest mod compiled against `guest_version` is SemVer-compatible with a
/// host running `host_version`.
///
/// Follows Cargo SemVer rules:
/// - Pre-1.0 (`major == 0`): minor bumps are breaking, so `host_major == 0`, `guest_major == 0`,
///   and `host_minor == guest_minor` are all required (patch differences are ignored).
/// - Post-1.0 (`major >= 1`): `host_major == guest_major` and `guest_minor <= host_minor`
///   (patch differences are ignored).
#[inline]
pub const fn abi_version_is_compatible(host_version: u32, guest_version: u32) -> bool {
    let (h_maj, h_min, _) = unpack_abi_version(host_version);
    let (g_maj, g_min, _) = unpack_abi_version(guest_version);
    if h_maj == 0 || g_maj == 0 {
        h_maj == 0 && g_maj == 0 && h_min == g_min
    } else {
        h_maj == g_maj && g_min <= h_min
    }
}
