//! Dynamic shared-library loader, shadow-copying (`LoadOptions`), RAII rollback transactions,
//! and `World`-scoped unload.

pub mod error;
pub mod mod_host;
pub mod schema_migration;
pub mod shared_lib;
#[cfg(feature = "signature-verification")]
#[forbid(unsafe_code)]
pub mod signing;
pub(crate) mod transaction;

pub use error::{ModLoadError, is_abi_version_compatible};
pub use mod_host::{
    LoadedMod, LoadedModInfo, ModHost, ModId, ModLoaded, ModScoped, ModUnloaded,
    excise_and_unload_mod_for_reload_in_world, excise_and_unload_mod_in_world, load_mod_in_world,
    record_mod_named_sets_in_world, restore_mod_state_in_world, rollback_unscheduled_mod_in_world,
    save_mod_state_in_world, unload_all_mods_in_world,
};
pub use schema_migration::{
    ExportedFieldRecord, ReloadColumnSnapshot, SnapshotComponentColumn, SnapshotEntityFields,
    cleanup_orphaned_mod_scoped_entities, migrate_mod_scoped_entities,
    restore_mod_dynamic_columns_after_reload, snapshot_mod_dynamic_columns_for_reload,
};
#[cfg(feature = "signature-verification")]
pub use shared_lib::preverify_mod_on_disk;
pub use shared_lib::{
    BinaryFormat, LibraryResidencyInfo, LoadOptions, canonical_mod_name_from_path,
    inspect_library_residency, inspect_library_residency_file,
};
#[cfg(feature = "signature-verification")]
pub use signing::{
    CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
    DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
    ModSigningError, ModTrustConfigError, ModTrustPolicy, SignatureEnforcement,
    SignatureVerificationError, VerifiedModSignature, default_signature_path_for,
    sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
};
