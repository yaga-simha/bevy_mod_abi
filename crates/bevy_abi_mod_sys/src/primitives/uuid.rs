//! Deterministic 128-bit component identifier.

/// Unique 128-bit component identifier (UUID-compatible).
///
/// Computed deterministically at compile-time via 2-lane 64-bit FNV-1a hashing.
///
/// # Example
///
/// ```
/// use bevy_abi_mod_sys::ComponentUuid;
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
