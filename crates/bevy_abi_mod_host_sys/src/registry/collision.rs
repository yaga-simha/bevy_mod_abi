//! Error diagnostics for conflicting component or resource registrations (`C-10`, `E-02`).

use bevy_abi_mod_sys::ComponentUuid;
use thiserror::Error;

/// Error returned when a component or resource registration collides with an existing entry's
/// 128-bit [`ComponentUuid`], canonical name, `(size, align)` layout, or storage/mutability flags.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ComponentCollisionError {
    /// Two distinct canonical names map to the same 128-bit [`ComponentUuid`].
    #[error(
        "ComponentUuid collision ({uuid:?}): '{new_name}' collides with already-registered '{existing_name}'"
    )]
    UuidCollision {
        /// Existing registered name bound to `uuid`.
        existing_name: String,
        /// Conflicting new name attempting to register under `uuid`.
        new_name: String,
        /// Colliding 128-bit UUID.
        uuid: ComponentUuid,
    },

    /// Re-registration of an existing canonical name with mismatched metadata.
    #[error("component/resource '{name}' re-registered with conflicting metadata: {reason}")]
    MetadataMismatch {
        /// Canonical component or resource name.
        name: String,
        /// Human-readable description of the mismatched metadata.
        reason: String,
    },
}
