//! Error diagnostics for conflicting component or resource registrations.

use bevy_abi_mod_sys::ComponentUuid;

/// Error returned when a component or resource registration collides with an existing entry's
/// 128-bit [`ComponentUuid`], canonical name, `(size, align)` layout, or component/resource kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComponentCollisionError {
    /// Two distinct canonical names map to the same 128-bit [`ComponentUuid`].
    UuidCollision {
        /// Existing registered name bound to `uuid`.
        existing_name: String,
        /// Conflicting new name attempting to register under `uuid`.
        new_name: String,
        /// Colliding 128-bit UUID.
        uuid: ComponentUuid,
    },
    /// Re-registration of an existing canonical name with mismatched metadata.
    MetadataMismatch {
        /// Canonical component or resource name.
        name: String,
        /// Human-readable description of the mismatched metadata.
        reason: String,
    },
}

impl core::fmt::Display for ComponentCollisionError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::UuidCollision {
                existing_name,
                new_name,
                uuid,
            } => write!(
                f,
                "ComponentUuid collision ({:?}): '{}' collides with already-registered '{}'",
                uuid.bytes, new_name, existing_name
            ),
            Self::MetadataMismatch { name, reason } => write!(
                f,
                "component/resource '{}' re-registered with conflicting metadata: {}",
                name, reason
            ),
        }
    }
}

impl std::error::Error for ComponentCollisionError {}
