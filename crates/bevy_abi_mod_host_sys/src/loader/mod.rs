//! Dynamic shared-library loader, shadow-copying (`LoadOptions`), RAII rollback transactions,
//! and `World`-scoped unload.

pub mod error;
pub mod mod_host;
pub mod shared_lib;
#[cfg(feature = "signature-verification")]
#[forbid(unsafe_code)]
pub mod signing;
pub(crate) mod transaction;

pub use error::{ModLoadError, is_abi_version_compatible};
pub use mod_host::{
    LoadedMod, LoadedModInfo, ModHost, ModId, ModLoaded, ModUnloaded,
    excise_and_unload_mod_in_world, load_mod_in_world, record_mod_named_sets_in_world,
    restore_mod_state_in_world, rollback_unscheduled_mod_in_world, save_mod_state_in_world,
    unload_all_mods_in_world,
};
#[cfg(feature = "signature-verification")]
pub use shared_lib::preverify_mod_on_disk;
pub use shared_lib::{LoadOptions, canonical_mod_name_from_path};
#[cfg(feature = "signature-verification")]
pub use signing::{
    CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
    DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
    ModSigningError, ModTrustConfigError, ModTrustPolicy, SignatureEnforcement,
    SignatureVerificationError, VerifiedModSignature, default_signature_path_for,
    sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
};
