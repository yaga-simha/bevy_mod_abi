//! Thin Bevy ECS adapters over [`bevy_abi_mod_trust`], storing cryptographic mod receipts
//! ([`VerifiedModSignature`]) and trust policies ([`ModTrustPolicy`]) as Bevy `Component` / `Resource`.

use bevy_ecs::prelude::{Component, Resource};
use core::ops::Deref;
use std::path::Path;

pub use bevy_abi_mod_trust::bampack::{
    BAMPACK_MAGIC, BAMPACK_VERSION, BamPackBuilder, BamPackEntry, DEFAULT_BAMPACK_NAMESPACE,
    MAX_BAMPACK_ENTRIES, VerifiedBamPack,
};
pub use bevy_abi_mod_trust::embedded_sig::{
    BAM_SIG_MACHO_SECTION_NAME, BAM_SIG_SECTION_NAME, EmbeddedSignatureSection,
    extract_embedded_bam_sig, zeroed_binary_for_bam_sig,
};
pub use bevy_abi_mod_trust::{
    CertificateValidityPolicy, DEFAULT_KRL_SIGNATURE_NAMESPACE, DEFAULT_MAX_MOD_BINARY_BYTES,
    DEFAULT_SIGNATURE_NAMESPACE, KrlBuilder, MAX_KRL_BYTES, MAX_SIGNATURE_SIDECAR_BYTES,
    ModSigningError, ModTrustConfigError, SignatureEnforcement, SignatureVerificationError,
    default_signature_path_for, sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
};

/// Verified cryptographic identity attached as an ECS [`Component`] to each spawned
/// [`LoadedModInfo`](crate::LoadedModInfo) entity (and stored on [`LoadedMod`](crate::LoadedMod))
/// when signature verification succeeds.
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct VerifiedModSignature {
    /// OpenSSH SHA-256 fingerprint (`"SHA256:..."`) of the leaf signing key.
    pub signer_fingerprint: String,
    /// OpenSSH SHA-256 fingerprint (`"SHA256:..."`) of the issuing Root CA, if certificate-backed.
    pub ca_fingerprint: Option<String>,
    /// Certificate `key_id` string assigned by the CA, if certificate-backed.
    pub key_id: Option<String>,
    /// Authorized principals embedded in the signing certificate, if certificate-backed.
    pub principals: Vec<String>,
    /// Certificate serial number assigned by the CA, if certificate-backed.
    pub serial: Option<u64>,
    /// Verified `SSHSIG` domain-separation namespace.
    pub namespace: String,
    /// SHA-256 digest of the exact verified shared-library bytes mapped into memory.
    pub binary_sha256: [u8; 32],
}

impl From<bevy_abi_mod_trust::VerifiedModSignature> for VerifiedModSignature {
    #[inline]
    fn from(v: bevy_abi_mod_trust::VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint,
            ca_fingerprint: v.ca_fingerprint,
            key_id: v.key_id,
            principals: v.principals,
            serial: v.serial,
            namespace: v.namespace,
            binary_sha256: v.binary_sha256,
        }
    }
}

impl From<&VerifiedModSignature> for bevy_abi_mod_trust::VerifiedModSignature {
    #[inline]
    fn from(v: &VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint.clone(),
            ca_fingerprint: v.ca_fingerprint.clone(),
            key_id: v.key_id.clone(),
            principals: v.principals.clone(),
            serial: v.serial,
            namespace: v.namespace.clone(),
            binary_sha256: v.binary_sha256,
        }
    }
}

impl From<VerifiedModSignature> for bevy_abi_mod_trust::VerifiedModSignature {
    #[inline]
    fn from(v: VerifiedModSignature) -> Self {
        Self {
            signer_fingerprint: v.signer_fingerprint,
            ca_fingerprint: v.ca_fingerprint,
            key_id: v.key_id,
            principals: v.principals,
            serial: v.serial,
            namespace: v.namespace,
            binary_sha256: v.binary_sha256,
        }
    }
}

/// Cryptographic trust policy configuring Ed25519 signature verification before `dlopen`.
///
/// Stored as a Bevy [`Resource`] in `World` (defaults to [`ModTrustPolicy::disabled`] when
/// omitted unless explicit enforcement is configured). Backed by zero-Bevy
/// [`bevy_abi_mod_trust::ModTrustPolicy`].
#[derive(Resource, Clone, Debug)]
pub struct ModTrustPolicy(bevy_abi_mod_trust::ModTrustPolicy);

impl Default for ModTrustPolicy {
    #[inline]
    fn default() -> Self {
        Self::disabled()
    }
}

impl Deref for ModTrustPolicy {
    type Target = bevy_abi_mod_trust::ModTrustPolicy;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl From<bevy_abi_mod_trust::ModTrustPolicy> for ModTrustPolicy {
    #[inline]
    fn from(inner: bevy_abi_mod_trust::ModTrustPolicy) -> Self {
        Self(inner)
    }
}

impl From<ModTrustPolicy> for bevy_abi_mod_trust::ModTrustPolicy {
    #[inline]
    fn from(policy: ModTrustPolicy) -> Self {
        policy.0
    }
}

impl ModTrustPolicy {
    /// Borrows the underlying pure-Rust [`bevy_abi_mod_trust::ModTrustPolicy`].
    #[inline]
    #[must_use]
    pub const fn as_inner(&self) -> &bevy_abi_mod_trust::ModTrustPolicy {
        &self.0
    }

    /// Unwraps into the underlying pure-Rust [`bevy_abi_mod_trust::ModTrustPolicy`].
    #[inline]
    #[must_use]
    pub fn into_inner(self) -> bevy_abi_mod_trust::ModTrustPolicy {
        self.0
    }

    /// Creates a policy with signature verification disabled ([`SignatureEnforcement::Disabled`]).
    #[must_use]
    pub fn disabled() -> Self {
        Self(bevy_abi_mod_trust::ModTrustPolicy::disabled())
    }

    /// Creates an enforcing ([`SignatureEnforcement::Required`]) policy with no trust anchors.
    #[must_use]
    pub fn strict() -> Self {
        Self(bevy_abi_mod_trust::ModTrustPolicy::strict())
    }

    /// Creates an enforcing policy trusting the given OpenSSH Ed25519 Certificate Authority public key.
    pub fn from_ca_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        bevy_abi_mod_trust::ModTrustPolicy::from_ca_openssh(openssh_pub).map(Self)
    }

    /// Creates an enforcing policy trusting the given pinned OpenSSH Ed25519 public key.
    pub fn from_public_key_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        bevy_abi_mod_trust::ModTrustPolicy::from_public_key_openssh(openssh_pub).map(Self)
    }

    /// Adds a trusted OpenSSH Ed25519 CA public key (deduplicated) and marks enforcement required.
    pub fn add_ca_openssh(self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        self.0.add_ca_openssh(openssh_pub).map(Self)
    }

    /// Reads and adds a trusted OpenSSH Ed25519 CA `.pub` file from disk.
    pub fn add_ca_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        self.0.add_ca_file(path).map(Self)
    }

    /// Adds a pinned OpenSSH Ed25519 public key (deduplicated) and marks enforcement required.
    pub fn add_public_key_openssh(self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        self.0.add_public_key_openssh(openssh_pub).map(Self)
    }

    /// Reads and adds a pinned OpenSSH Ed25519 `.pub` file from disk.
    pub fn add_public_key_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        self.0.add_public_key_file(path).map(Self)
    }

    /// Sets [`SignatureEnforcement`] explicitly.
    #[must_use]
    pub fn with_enforcement(self, enforcement: SignatureEnforcement) -> Self {
        Self(self.0.with_enforcement(enforcement))
    }

    /// Sets the expected `SSHSIG` domain-separation namespace (rejects empty strings).
    pub fn with_namespace(self, namespace: impl Into<String>) -> Result<Self, ModTrustConfigError> {
        self.0.with_namespace(namespace).map(Self)
    }

    /// Requires certificate-backed signatures to authorize `principal` in `valid_principals`.
    #[must_use]
    pub fn with_required_principal(self, principal: impl Into<String>) -> Self {
        Self(self.0.with_required_principal(principal))
    }

    /// Requires `BamManifest::name` to appear in certificate `valid_principals`.
    #[must_use]
    pub fn with_manifest_name_principal_binding(self, enabled: bool) -> Self {
        Self(self.0.with_manifest_name_principal_binding(enabled))
    }

    /// Sets the certificate validity time-checking policy.
    #[must_use]
    pub fn with_validity_policy(self, policy: CertificateValidityPolicy) -> Self {
        Self(self.0.with_validity_policy(policy))
    }

    /// Sets the maximum mod shared-library file size buffered into memory for verification.
    #[must_use]
    pub fn with_max_binary_bytes(self, max_bytes: u64) -> Self {
        Self(self.0.with_max_binary_bytes(max_bytes))
    }

    /// Revokes a certificate serial number across all trusted CAs.
    #[must_use]
    pub fn revoke_serial(self, serial: u64) -> Self {
        Self(self.0.revoke_serial(serial))
    }

    /// Revokes an inclusive certificate serial range `[min, max]` across all trusted CAs.
    pub fn revoke_serial_range(self, min: u64, max: u64) -> Result<Self, ModTrustConfigError> {
        self.0.revoke_serial_range(min, max).map(Self)
    }

    /// Revokes a certificate `key_id` across all trusted CAs.
    #[must_use]
    pub fn revoke_key_id(self, key_id: impl Into<String>) -> Self {
        Self(self.0.revoke_key_id(key_id))
    }

    /// Revokes a leaf or CA OpenSSH Ed25519 public key.
    pub fn revoke_key_openssh(self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        self.0.revoke_key_openssh(openssh_pub).map(Self)
    }

    /// Revokes a SHA-256 OpenSSH key fingerprint (`"SHA256:..."`).
    pub fn revoke_fingerprint(self, fingerprint: &str) -> Result<Self, ModTrustConfigError> {
        self.0.revoke_fingerprint(fingerprint).map(Self)
    }

    /// Enforces a minimum rollback version on signed KRLs applied via [`Self::add_signed_krl_bytes`].
    #[must_use]
    pub fn with_min_krl_version(self, min_version: u64) -> Self {
        Self(self.0.with_min_krl_version(min_version))
    }

    /// Merges revocations from a binary OpenSSH Key Revocation List (`SSHKRL`).
    pub fn add_krl_bytes(self, krl_bytes: &[u8]) -> Result<Self, ModTrustConfigError> {
        self.0.add_krl_bytes(krl_bytes).map(Self)
    }

    /// Reads and merges a binary `SSHKRL` file from disk.
    pub fn add_krl_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        self.0.add_krl_file(path).map(Self)
    }

    /// Verifies and merges a signed `SSHKRL` along with its OpenSSH `SSHSIG` envelope.
    pub fn add_signed_krl_bytes(
        self,
        krl_bytes: &[u8],
        sig_pem_bytes: &[u8],
    ) -> Result<Self, ModTrustConfigError> {
        self.0
            .add_signed_krl_bytes(krl_bytes, sig_pem_bytes)
            .map(Self)
    }

    /// Reads, verifies, and merges a signed `SSHKRL` and its `.sig` sidecar from disk.
    pub fn add_signed_krl_file(
        self,
        krl_path: impl AsRef<Path>,
        sig_path: Option<&Path>,
    ) -> Result<Self, ModTrustConfigError> {
        self.0.add_signed_krl_file(krl_path, sig_path).map(Self)
    }

    /// Parses and merges an OpenSSH KRL text revocation specification (`serial:`, `key:`, `sha256:`, `id:`).
    pub fn add_revocation_spec_str(self, spec: &str) -> Result<Self, ModTrustConfigError> {
        self.0.add_revocation_spec_str(spec).map(Self)
    }

    /// Verifies `binary_bytes` against detached `sig_pem_bytes`.
    pub fn verify_bytes(
        &self,
        binary_bytes: &[u8],
        sig_pem_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_bytes(binary_bytes, sig_pem_bytes)
            .map(VerifiedModSignature::from)
    }

    /// Verifies `binary_bytes` using an embedded `.bam_sig` / `__bam_sig` section.
    pub fn verify_embedded_bam_sig(
        &self,
        binary_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_embedded_bam_sig(binary_bytes)
            .map(VerifiedModSignature::from)
    }

    /// Verifies `binary_bytes` using a detached sidecar when `Some`, or embedded `.bam_sig` when `None`.
    pub fn verify_binary_or_sidecar(
        &self,
        binary_bytes: &[u8],
        sidecar_sig: Option<&[u8]>,
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        self.0
            .verify_binary_or_sidecar(binary_bytes, sidecar_sig)
            .map(VerifiedModSignature::from)
    }

    /// Verifies a `.bampack` archive in memory under this policy.
    pub fn verify_bampack_bytes(
        &self,
        pack_bytes: &[u8],
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        self.0.verify_bampack_bytes(pack_bytes)
    }

    /// Reads and verifies a `.bampack` archive from disk under this policy.
    pub fn verify_bampack_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        self.0.verify_bampack_file(path)
    }

    /// Re-validates an already-issued [`VerifiedModSignature`] receipt against active anchors/KRLs.
    pub fn verify_receipt_still_trusted(
        &self,
        receipt: &VerifiedModSignature,
    ) -> Result<(), SignatureVerificationError> {
        let trust_receipt = bevy_abi_mod_trust::VerifiedModSignature::from(receipt);
        self.0.verify_receipt_still_trusted(&trust_receipt)
    }

    /// Checks [`VerifiedModSignature::principals`] against `manifest_name` when principal binding is enabled.
    pub fn verify_manifest_principal(
        &self,
        receipt: &VerifiedModSignature,
        manifest_name: &str,
    ) -> Result<(), SignatureVerificationError> {
        let trust_receipt = bevy_abi_mod_trust::VerifiedModSignature::from(receipt);
        self.0
            .verify_manifest_principal(&trust_receipt, manifest_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_ecs::world::World;
    use ssh_key::{Algorithm, PrivateKey, rand_core::OsRng};

    #[test]
    fn host_sys_trust_ecs_adapters_store_and_verify() {
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let pub_str = key.public_key().to_openssh().unwrap();
        let policy = ModTrustPolicy::from_public_key_openssh(&pub_str).unwrap();
        let payload = b"host-sys-adapter-payload";
        let sig = sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
        let receipt = policy.verify_bytes(payload, sig.as_bytes()).unwrap();
        policy.verify_receipt_still_trusted(&receipt).unwrap();

        let mut world = World::new();
        world.insert_resource(policy);
        let entity = world.spawn(receipt.clone()).id();
        assert_eq!(world.get::<VerifiedModSignature>(entity), Some(&receipt));
        assert!(world.resource::<ModTrustPolicy>().has_trust_anchors());
    }
}
