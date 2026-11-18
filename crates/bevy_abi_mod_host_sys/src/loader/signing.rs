//! Pure-Rust OpenSSH `SSHSIG` and OpenSSH Ed25519 Certificate Authority verification
//! for native shared-library mods (`signature-verification` feature).

use bevy_ecs::component::Component;
use bevy_ecs::prelude::Resource;
use signature::Signer;
use ssh_encoding::pem::PemLabel;
use ssh_encoding::{CheckedSum, Decode, DecodePem, Encode, EncodePem, LineEnding, Reader, Writer};
use ssh_key::certificate::{CertType, OptionsMap};
use ssh_key::public::{Ed25519PublicKey, KeyData};
use ssh_key::sha2::{Digest, Sha256};
use ssh_key::{Algorithm, Certificate, Fingerprint, HashAlg, PrivateKey, PublicKey, Signature};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Default `SSHSIG` domain-separation namespace required on mod signatures.
pub const DEFAULT_SIGNATURE_NAMESPACE: &str = "bevy_abi_mod.v1";

/// Default `SSHSIG` domain-separation namespace required on signed KRLs (`add_signed_krl_*`).
pub const DEFAULT_KRL_SIGNATURE_NAMESPACE: &str = "bevy_abi_mod.krl.v1";

/// Default upper bound (`256 MiB`) on mod shared-library bytes buffered for verification.
pub const DEFAULT_MAX_MOD_BINARY_BYTES: u64 = 256 * 1024 * 1024;

/// Maximum allowed size (`64 KiB`) for a detached `.sig` sidecar file.
pub const MAX_SIGNATURE_SIDECAR_BYTES: u64 = 64 * 1024;

/// Maximum allowed size (`16 MiB`) for a binary OpenSSH Key Revocation List (`.krl`) file.
pub const MAX_KRL_BYTES: u64 = 16 * 1024 * 1024;

const SSHSIG_MAGIC_PREAMBLE: &[u8] = b"SSHSIG";
const SSHSIG_SUPPORTED_VERSION: u32 = 1;
const ED25519_CERT_ALGORITHM_ID: &str = "ssh-ed25519-cert-v01@openssh.com";

const SSHKRL_MAGIC: &[u8; 8] = b"SSHKRL\n\0";
const SSHKRL_SUPPORTED_FORMAT_VERSION: u32 = 1;
const KRL_SECTION_CERTIFICATES: u8 = 0x01;
const KRL_SECTION_EXPLICIT_KEY: u8 = 0x02;
const KRL_SECTION_FINGERPRINT_SHA1: u8 = 0x03;
const KRL_SECTION_SIGNATURE: u8 = 0x04;
const KRL_SECTION_FINGERPRINT_SHA256: u8 = 0x05;
const KRL_SECTION_EXTENSION: u8 = 255;
const KRL_SECTION_CERT_SERIAL_LIST: u8 = 0x20;
const KRL_SECTION_CERT_SERIAL_RANGE: u8 = 0x21;
const KRL_SECTION_CERT_SERIAL_BITMAP: u8 = 0x22;
const KRL_SECTION_CERT_KEY_ID: u8 = 0x23;
const KRL_SECTION_CERT_EXTENSION: u8 = 255;

/// Derives the default detached signature sidecar path for `mod_path` by appending `".sig"`
/// (`foo.dll` -> `foo.dll.sig`, `libfoo.dylib` -> `libfoo.dylib.sig`).
#[must_use]
pub fn default_signature_path_for(mod_path: &Path) -> PathBuf {
    let mut os = mod_path.as_os_str().to_os_string();
    os.push(".sig");
    PathBuf::from(os)
}

/// Controls whether cryptographic signature verification is enforced during mod loading.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SignatureEnforcement {
    /// Signatures are not verified on load.
    #[default]
    Disabled,
    /// Every mod load/reload must pass cryptographic verification against `ModTrustPolicy`.
    Required,
}

/// Controls how certificate `valid_after` / `valid_before` timestamps are checked.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CertificateValidityPolicy {
    /// Checks `valid_after <= now < valid_before` using `std::time::SystemTime::now()`.
    #[default]
    SystemClock,
    /// Checks `valid_after <= timestamp < valid_before` using an explicit Unix epoch timestamp.
    FixedTimestamp(u64),
    /// Ignores wall-clock time comparisons while still verifying CA signatures, non-inverted
    /// bounds (`valid_after < valid_before`), key types, critical options, principals, and revocations.
    IgnoreTimestamps,
}

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

/// Errors returned when constructing or mutating a [`ModTrustPolicy`].
#[derive(Debug, Error)]
pub enum ModTrustConfigError {
    /// Failed to parse an OpenSSH public key string.
    #[error("invalid OpenSSH public key: {detail}")]
    InvalidPublicKey {
        /// Human-readable parse error detail.
        detail: String,
    },

    /// Public key uses an algorithm other than `ssh-ed25519`.
    #[error("unsupported key algorithm '{algorithm}' (only 'ssh-ed25519' is supported)")]
    UnsupportedKeyAlgorithm {
        /// Algorithm string found on the key.
        algorithm: String,
    },

    /// Failed to parse an OpenSSH key fingerprint string.
    #[error("invalid OpenSSH fingerprint '{fingerprint}': {detail}")]
    InvalidFingerprint {
        /// Input fingerprint string.
        fingerprint: String,
        /// Parse/validation failure detail.
        detail: String,
    },

    /// Failed to read a trust-anchor key file from disk.
    #[error("failed to read key file '{path}': {source}")]
    Io {
        /// Key file path.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Configured `SSHSIG` namespace was empty.
    #[error("signature namespace must not be empty")]
    EmptyNamespace,

    /// Ed25519 public key failed strict Curve25519 subgroup or canonical encoding validation.
    #[error("weak or non-canonical Ed25519 public key: {detail}")]
    WeakOrInvalidPublicKey {
        /// Curve/encoding validation failure detail.
        detail: String,
    },

    /// Binary or text Key Revocation List (KRL) failed structural or range validation.
    #[error("invalid OpenSSH key revocation list: {detail}")]
    InvalidRevocationList {
        /// Structural parsing or validation detail.
        detail: String,
    },

    /// Signed KRL failed cryptographic `SSHSIG` verification against configured trust anchors.
    #[error("signed KRL failed cryptographic verification: {detail}")]
    KrlSignatureInvalid {
        /// Verification failure detail.
        detail: String,
    },

    /// Offered KRL version is strictly older than the configured floor or already-applied KRL.
    #[error("KRL version {offered} precedes minimum required version {min_required}")]
    KrlVersionRollback {
        /// Version embedded in the rejected KRL.
        offered: u64,
        /// Minimum accepted KRL version.
        min_required: u64,
    },
}

/// Errors returned when verifying a mod shared library or its detached `.sig` sidecar.
#[derive(Debug, Error)]
pub enum SignatureVerificationError {
    /// Enforcement is active, but no CA or pinned public keys were configured.
    #[error(
        "signature verification is required but ModTrustPolicy has no trust anchors configured"
    )]
    NoTrustAnchorsConfigured,

    /// Failed to read the mod shared library file prior to verification.
    #[error("failed to read mod binary for verification: {source}")]
    SourceBinaryRead {
        /// Underlying filesystem I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Mod shared library is empty (`0` bytes).
    #[error("mod binary is empty (0 bytes)")]
    EmptyBinary,

    /// Mod shared library exceeded `ModTrustPolicy::max_binary_bytes()`.
    #[error("mod binary size ({size} bytes) exceeds configured limit ({limit} bytes)")]
    BinaryTooLarge {
        /// Observed size (or `limit + 1` when capped during bounded read).
        size: u64,
        /// Configured maximum size in bytes.
        limit: u64,
    },

    /// Detached `.sig` sidecar file was missing.
    #[error("missing detached signature file: {source}")]
    MissingSignatureFile {
        /// Underlying `NotFound` I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Failed to read the detached `.sig` sidecar file.
    #[error("failed to read detached signature file: {source}")]
    SignatureFileRead {
        /// Underlying filesystem I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Detached `.sig` sidecar exceeded [`MAX_SIGNATURE_SIDECAR_BYTES`].
    #[error("signature file size ({size} bytes) exceeds maximum ({limit} bytes)")]
    SignatureFileTooLarge {
        /// Observed size (or `limit + 1` when capped during bounded read).
        size: u64,
        /// Maximum permitted `.sig` file size.
        limit: u64,
    },

    /// Detached `.sig` PEM or embedded OpenSSH wire encoding was malformed.
    #[error("malformed SSHSIG signature: {detail}")]
    MalformedSignature {
        /// Structural parsing detail.
        detail: String,
    },

    /// Signature, signing key, or certificate used an algorithm other than Ed25519.
    #[error("unsupported cryptographic algorithm in signature: {detail}")]
    UnsupportedAlgorithm {
        /// Algorithm mismatch detail.
        detail: String,
    },

    /// `SSHSIG` namespace did not match [`ModTrustPolicy::namespace()`].
    #[error("signature namespace mismatch (expected '{expected}', found '{actual}')")]
    NamespaceMismatch {
        /// Expected namespace from policy.
        expected: String,
        /// Actual namespace recorded inside `.sig`.
        actual: String,
    },

    /// Embedded certificate was signed by an untrusted CA or failed CA signature verification.
    #[error("untrusted or invalid certificate authority '{ca_fingerprint}'")]
    UntrustedCertificateAuthority {
        /// SHA-256 fingerprint of the certificate's declared CA key.
        ca_fingerprint: String,
    },

    /// Direct public-key signature did not match any pinned public key in [`ModTrustPolicy`].
    #[error("untrusted signing public key '{fingerprint}'")]
    UntrustedPublicKey {
        /// SHA-256 fingerprint of the signing key.
        fingerprint: String,
    },

    /// Embedded certificate was not an OpenSSH User certificate (`CertType::User`).
    #[error("invalid certificate type: {detail}")]
    InvalidCertificateType {
        /// Certificate type detail.
        detail: String,
    },

    /// Embedded certificate carried unrecognized critical options.
    #[error("certificate contains unrecognized critical options: {}", options.join(", "))]
    UnrecognizedCriticalOption {
        /// Critical option names present on the certificate.
        options: Vec<String>,
    },

    /// Certificate validity window did not contain the policy's verification timestamp.
    #[error(
        "certificate not valid at unix time {check_time} (valid range: [{valid_after}, {valid_before}))"
    )]
    CertificateExpiredOrNotYetValid {
        /// Certificate `valid_after` timestamp (inclusive).
        valid_after: u64,
        /// Certificate `valid_before` timestamp (exclusive).
        valid_before: u64,
        /// Timestamp evaluated against the validity window.
        check_time: u64,
    },

    /// Signing key or issuing CA key fingerprint has been explicitly revoked.
    #[error("signer or CA key '{fingerprint}' has been revoked")]
    SignerRevoked {
        /// Revoked SHA-256 key fingerprint.
        fingerprint: String,
    },

    /// Certificate serial number has been explicitly revoked.
    #[error("certificate serial {serial} has been revoked")]
    CertificateSerialRevoked {
        /// Revoked certificate serial number.
        serial: u64,
    },

    /// Certificate `key_id` has been explicitly revoked.
    #[error("certificate key ID '{key_id}' has been revoked")]
    CertificateKeyIdRevoked {
        /// Revoked certificate `key_id` string.
        key_id: String,
    },

    /// Signing key, leaf key, or CA key failed strict Ed25519 subgroup/canonical validation.
    #[error("weak or non-canonical Ed25519 public key in signature or certificate: {detail}")]
    WeakOrInvalidPublicKey {
        /// Curve/encoding validation failure detail.
        detail: String,
    },

    /// Certificate principals did not authorize the required principal or mod manifest name.
    #[error(
        "certificate principal mismatch: expected '{expected}', authorized principals: [{}]",
        actual.join(", ")
    )]
    PrincipalMismatch {
        /// Required principal or `BamManifest::name`.
        expected: String,
        /// Principals listed on the signing certificate.
        actual: Vec<String>,
    },

    /// Cryptographic Ed25519 signature verification failed against the binary bytes.
    #[error("cryptographic signature verification failed")]
    InvalidSignature,
}

/// Errors returned when producing OpenSSH `SSHSIG` detached signatures in pure Rust.
#[derive(Debug, Error)]
pub enum ModSigningError {
    /// Provided namespace string was empty.
    #[error("signature namespace must not be empty")]
    EmptyNamespace,

    /// Provided signing key or certificate did not use Ed25519.
    #[error("unsupported algorithm '{detail}' (only Ed25519 is supported)")]
    UnsupportedAlgorithm {
        /// Algorithm mismatch detail.
        detail: String,
    },

    /// Signing private key did not match the supplied certificate's embedded public key.
    #[error("signing private key does not match the supplied certificate public key")]
    MismatchedCertificatePublicKey,

    /// Certificate OpenSSH string or binary encoding was malformed.
    #[error("failed to parse or encode SSHSIG/certificate wire bytes: {detail}")]
    Encoding {
        /// Encoding failure detail.
        detail: String,
    },

    /// Signing operation failed.
    #[error("failed to compute Ed25519 SSHSIG signature: {detail}")]
    Signing {
        /// Underlying signing error detail.
        detail: String,
    },
}

/// World-level cryptographic trust policy governing load-time verification of native mods.
///
/// Stored as a Bevy [`Resource`] in `World` (defaults to [`ModTrustPolicy::disabled`] when
/// initialized without an explicit policy). When [`SignatureEnforcement::Required`] is active,
/// `ModHost` verifies detached `.sig` sidecars in memory and stages verified bytes into an
/// exclusively created shadow file before invoking `dlopen` / `LoadLibraryW`.
#[derive(Clone, Debug, Default)]
struct SerialBitmap {
    offset: u64,
    big_endian_bytes: Vec<u8>,
}

impl SerialBitmap {
    fn contains(&self, serial: u64) -> bool {
        let Some(delta) = serial.checked_sub(self.offset) else {
            return false;
        };
        let Ok(byte_from_lsb) = usize::try_from(delta / 8) else {
            return false;
        };
        if byte_from_lsb >= self.big_endian_bytes.len() {
            return false;
        }
        let byte = self.big_endian_bytes[self.big_endian_bytes.len() - 1 - byte_from_lsb];
        let bit = (delta % 8) as u8;
        ((byte >> bit) & 1) != 0
    }
}

#[derive(Clone, Debug, Default)]
struct CertRevocationScope {
    serials: HashSet<u64>,
    serial_ranges: Vec<(u64, u64)>,
    serial_bitmaps: Vec<SerialBitmap>,
    key_ids: HashSet<String>,
}

impl CertRevocationScope {
    fn is_serial_revoked(&self, serial: u64) -> bool {
        self.serials.contains(&serial)
            || self
                .serial_ranges
                .iter()
                .any(|&(min, max)| min <= serial && serial <= max)
            || self.serial_bitmaps.iter().any(|bm| bm.contains(serial))
    }

    fn is_key_id_revoked(&self, key_id: &str) -> bool {
        self.key_ids.contains(key_id)
    }

    fn merge(&mut self, other: Self) {
        self.serials.extend(other.serials);
        self.serial_ranges.extend(other.serial_ranges);
        self.serial_bitmaps.extend(other.serial_bitmaps);
        self.key_ids.extend(other.key_ids);
    }
}

/// World-level cryptographic trust policy governing load-time verification of native mods.
///
/// Stored as a Bevy [`Resource`] in `World` (defaults to [`ModTrustPolicy::disabled`] when
/// initialized without an explicit policy). When [`SignatureEnforcement::Required`] is active,
/// `ModHost` verifies detached `.sig` sidecars in memory and stages verified bytes into an
/// exclusively created shadow file before invoking `dlopen` / `LoadLibraryW`.
#[derive(Resource, Clone, Debug)]
pub struct ModTrustPolicy {
    enforcement: SignatureEnforcement,
    trusted_ca_keys: Vec<PublicKey>,
    trusted_public_keys: Vec<PublicKey>,
    namespace: String,
    required_principal: Option<String>,
    bind_manifest_name_to_principal: bool,
    validity_policy: CertificateValidityPolicy,
    max_binary_bytes: u64,
    revoked_key_fingerprints: BTreeSet<Fingerprint>,
    global_cert_revocations: CertRevocationScope,
    ca_cert_revocations: BTreeMap<Fingerprint, CertRevocationScope>,
    min_krl_version: u64,
    applied_krl_version: Option<u64>,
}

impl Default for ModTrustPolicy {
    #[inline]
    fn default() -> Self {
        Self::disabled()
    }
}

fn is_canonical_ed25519_y(raw: &[u8; 32]) -> bool {
    let mut y_bytes = *raw;
    // Bit 255 stores the x-coordinate sign bit; bits 0..=254 encode y in little-endian.
    y_bytes[31] &= 0x7f;
    let limbs = [
        u64::from_le_bytes(y_bytes[0..8].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[8..16].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[16..24].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[24..32].try_into().unwrap()),
    ];
    // Curve25519 field prime p = 2^255 - 19 = 0x7fffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffed.
    const P_LIMBS: [u64; 4] = [
        0xffff_ffff_ffff_ffed,
        u64::MAX,
        u64::MAX,
        0x7fff_ffff_ffff_ffff,
    ];
    (limbs[3], limbs[2], limbs[1], limbs[0]) < (P_LIMBS[3], P_LIMBS[2], P_LIMBS[1], P_LIMBS[0])
}

fn validate_strict_ed25519_kd(kd: &KeyData) -> Result<ed25519_dalek::VerifyingKey, String> {
    let raw = match kd {
        KeyData::Ed25519(pk) => &pk.0,
        other => {
            return Err(format!(
                "expected Ed25519 key, found '{}'",
                other.algorithm()
            ));
        }
    };
    if !is_canonical_ed25519_y(raw) {
        return Err("non-canonical compressed Ed25519 public key encoding (y >= p)".to_owned());
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(raw)
        .map_err(|e| format!("invalid Curve25519 point: {e}"))?;
    if vk.is_weak() {
        return Err("small-order Curve25519 torsion public key is rejected".to_owned());
    }
    Ok(vk)
}

enum StrictSigVerifyError {
    WeakOrInvalidKey(String),
    InvalidSignature,
}

fn verify_strict_ed25519_sig(
    kd: &KeyData,
    msg: &[u8],
    sig: &Signature,
) -> Result<(), StrictSigVerifyError> {
    let vk = validate_strict_ed25519_kd(kd).map_err(StrictSigVerifyError::WeakOrInvalidKey)?;
    if sig.algorithm() != Algorithm::Ed25519 {
        return Err(StrictSigVerifyError::InvalidSignature);
    }
    let sig_bytes: [u8; 64] = sig
        .as_bytes()
        .try_into()
        .map_err(|_| StrictSigVerifyError::InvalidSignature)?;
    let dalek_sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    vk.verify_strict(msg, &dalek_sig)
        .map_err(|_| StrictSigVerifyError::InvalidSignature)
}

fn parse_ed25519_openssh_key(openssh_pub: &str) -> Result<PublicKey, ModTrustConfigError> {
    let trimmed = openssh_pub.trim();
    let key =
        PublicKey::from_openssh(trimmed).map_err(|e| ModTrustConfigError::InvalidPublicKey {
            detail: e.to_string(),
        })?;
    if key.algorithm() != Algorithm::Ed25519 {
        return Err(ModTrustConfigError::UnsupportedKeyAlgorithm {
            algorithm: key.algorithm().to_string(),
        });
    }
    validate_strict_ed25519_kd(key.key_data())
        .map_err(|detail| ModTrustConfigError::WeakOrInvalidPublicKey { detail })?;
    Ok(key)
}

fn read_bounded_config_bytes(path: &Path, max_bytes: u64) -> Result<Vec<u8>, ModTrustConfigError> {
    let file = std::fs::File::open(path).map_err(|source| ModTrustConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let meta = file.metadata().map_err(|source| ModTrustConfigError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if !meta.is_file() {
        return Err(ModTrustConfigError::Io {
            path: path.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "expected a regular file",
            ),
        });
    }
    if meta.len() > max_bytes {
        return Err(ModTrustConfigError::InvalidRevocationList {
            detail: format!(
                "file '{}' ({} bytes) exceeds limit ({max_bytes} bytes)",
                path.display(),
                meta.len()
            ),
        });
    }
    let cap = usize::try_from(meta.len()).unwrap_or(64 * 1024);
    let mut buf = Vec::with_capacity(cap);
    let read_limit = max_bytes.saturating_add(1);
    std::io::Read::read_to_end(&mut std::io::Read::take(file, read_limit), &mut buf).map_err(
        |source| ModTrustConfigError::Io {
            path: path.to_path_buf(),
            source,
        },
    )?;
    if buf.len() as u64 > max_bytes {
        return Err(ModTrustConfigError::InvalidRevocationList {
            detail: format!(
                "file '{}' exceeded byte limit ({max_bytes} bytes) while reading",
                path.display()
            ),
        });
    }
    Ok(buf)
}

impl ModTrustPolicy {
    /// Creates a policy with signature verification disabled (`SignatureEnforcement::Disabled`).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enforcement: SignatureEnforcement::Disabled,
            trusted_ca_keys: Vec::new(),
            trusted_public_keys: Vec::new(),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            required_principal: None,
            bind_manifest_name_to_principal: false,
            validity_policy: CertificateValidityPolicy::SystemClock,
            max_binary_bytes: DEFAULT_MAX_MOD_BINARY_BYTES,
            revoked_key_fingerprints: BTreeSet::new(),
            global_cert_revocations: CertRevocationScope::default(),
            ca_cert_revocations: BTreeMap::new(),
            min_krl_version: 0,
            applied_krl_version: None,
        }
    }

    /// Creates an enforcing (`SignatureEnforcement::Required`) policy with no trust anchors.
    ///
    /// Fails closed with [`SignatureVerificationError::NoTrustAnchorsConfigured`] until at least
    /// one CA or pinned public key is added.
    #[must_use]
    pub fn strict() -> Self {
        Self {
            enforcement: SignatureEnforcement::Required,
            ..Self::disabled()
        }
    }

    /// Creates an enforcing policy trusting the given OpenSSH Ed25519 Certificate Authority
    /// public key (`"ssh-ed25519 AAAA... ..."`).
    pub fn from_ca_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        Self::strict().add_ca_openssh(openssh_pub)
    }

    /// Creates an enforcing policy trusting the given pinned OpenSSH Ed25519 public key
    /// (`"ssh-ed25519 AAAA... ..."`).
    pub fn from_public_key_openssh(openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        Self::strict().add_public_key_openssh(openssh_pub)
    }

    /// Adds a trusted OpenSSH Ed25519 CA public key (deduplicated) and marks enforcement as required.
    pub fn add_ca_openssh(mut self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        let key = parse_ed25519_openssh_key(openssh_pub)?;
        if !self
            .trusted_ca_keys
            .iter()
            .any(|existing| existing.key_data() == key.key_data())
        {
            self.trusted_ca_keys.push(key);
        }
        self.enforcement = SignatureEnforcement::Required;
        Ok(self)
    }

    /// Reads and adds a trusted OpenSSH Ed25519 CA `.pub` file from disk.
    pub fn add_ca_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        let path_ref = path.as_ref();
        let text = std::fs::read_to_string(path_ref).map_err(|source| ModTrustConfigError::Io {
            path: path_ref.to_path_buf(),
            source,
        })?;
        self.add_ca_openssh(&text)
    }

    /// Adds a pinned OpenSSH Ed25519 public key (deduplicated) and marks enforcement as required.
    pub fn add_public_key_openssh(
        mut self,
        openssh_pub: &str,
    ) -> Result<Self, ModTrustConfigError> {
        let key = parse_ed25519_openssh_key(openssh_pub)?;
        if !self
            .trusted_public_keys
            .iter()
            .any(|existing| existing.key_data() == key.key_data())
        {
            self.trusted_public_keys.push(key);
        }
        self.enforcement = SignatureEnforcement::Required;
        Ok(self)
    }

    /// Reads and adds a pinned OpenSSH Ed25519 `.pub` file from disk.
    pub fn add_public_key_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        let path_ref = path.as_ref();
        let text = std::fs::read_to_string(path_ref).map_err(|source| ModTrustConfigError::Io {
            path: path_ref.to_path_buf(),
            source,
        })?;
        self.add_public_key_openssh(&text)
    }

    /// Sets [`SignatureEnforcement`] explicitly.
    #[must_use]
    pub const fn with_enforcement(mut self, enforcement: SignatureEnforcement) -> Self {
        self.enforcement = enforcement;
        self
    }

    /// Sets the expected `SSHSIG` domain-separation namespace (rejects empty strings).
    pub fn with_namespace(
        mut self,
        namespace: impl Into<String>,
    ) -> Result<Self, ModTrustConfigError> {
        let ns = namespace.into();
        if ns.is_empty() {
            return Err(ModTrustConfigError::EmptyNamespace);
        }
        self.namespace = ns;
        Ok(self)
    }

    /// Requires certificate-backed signatures to authorize `principal` in `valid_principals`.
    #[must_use]
    pub fn with_required_principal(mut self, principal: impl Into<String>) -> Self {
        self.required_principal = Some(principal.into());
        self
    }

    /// When `enabled`, verifies after `bevy_abi_mod_manifest()` (and before `bevy_abi_mod_init`)
    /// that the mod's declared `BamManifest::name` is present in the certificate's
    /// `valid_principals`.
    ///
    /// Note: Pinned public-key signatures (without a certificate) have an empty principal list,
    /// so enabling this rejects raw key signatures with [`SignatureVerificationError::PrincipalMismatch`].
    #[must_use]
    pub const fn with_manifest_name_principal_binding(mut self, enabled: bool) -> Self {
        self.bind_manifest_name_to_principal = enabled;
        self
    }

    /// Sets the certificate validity time-checking policy (`SystemClock`, `FixedTimestamp`, or
    /// `IgnoreTimestamps`).
    #[must_use]
    pub const fn with_validity_policy(mut self, policy: CertificateValidityPolicy) -> Self {
        self.validity_policy = policy;
        self
    }

    /// Sets the maximum mod shared-library file size buffered into memory for verification.
    #[must_use]
    pub const fn with_max_binary_bytes(mut self, max_bytes: u64) -> Self {
        self.max_binary_bytes = max_bytes;
        self
    }

    /// Revokes a certificate serial number across all trusted CAs.
    #[must_use]
    pub fn revoke_serial(mut self, serial: u64) -> Self {
        self.global_cert_revocations.serials.insert(serial);
        self
    }

    /// Revokes an inclusive certificate serial range `[min, max]` across all trusted CAs.
    pub fn revoke_serial_range(mut self, min: u64, max: u64) -> Result<Self, ModTrustConfigError> {
        if min > max {
            return Err(ModTrustConfigError::InvalidRevocationList {
                detail: format!("serial range bounds inverted: {min} > {max}"),
            });
        }
        self.global_cert_revocations.serial_ranges.push((min, max));
        Ok(self)
    }

    /// Revokes a certificate `key_id` across all trusted CAs.
    #[must_use]
    pub fn revoke_key_id(mut self, key_id: impl Into<String>) -> Self {
        self.global_cert_revocations.key_ids.insert(key_id.into());
        self
    }

    /// Revokes a key by its OpenSSH public key string (`"ssh-ed25519 ..."`).
    pub fn revoke_key_openssh(mut self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        let key = parse_ed25519_openssh_key(openssh_pub)?;
        self.revoked_key_fingerprints
            .insert(key.fingerprint(HashAlg::Sha256));
        Ok(self)
    }

    /// Revokes a key by its OpenSSH SHA-256 fingerprint string (`"SHA256:..."`).
    pub fn revoke_fingerprint(mut self, fingerprint: &str) -> Result<Self, ModTrustConfigError> {
        let trimmed = fingerprint.trim();
        let fp: Fingerprint = trimmed.parse().map_err(|e: ssh_key::Error| {
            ModTrustConfigError::InvalidFingerprint {
                fingerprint: trimmed.to_owned(),
                detail: e.to_string(),
            }
        })?;
        if !fp.is_sha256() {
            return Err(ModTrustConfigError::InvalidFingerprint {
                fingerprint: trimmed.to_owned(),
                detail: "only SHA256 fingerprints are supported".to_owned(),
            });
        }
        self.revoked_key_fingerprints.insert(fp);
        Ok(self)
    }

    /// Enforces a minimum acceptable OpenSSH KRL version (`krl_version >= min_version`) to block
    /// replay/rollback to older revocation lists.
    #[must_use]
    pub const fn with_min_krl_version(mut self, min_version: u64) -> Self {
        self.min_krl_version = min_version;
        self
    }

    /// Returns the configured minimum accepted KRL version floor.
    #[must_use]
    pub const fn min_krl_version(&self) -> u64 {
        self.min_krl_version
    }

    /// Returns the highest KRL version merged into this policy, if any binary KRL was loaded.
    #[must_use]
    pub const fn applied_krl_version(&self) -> Option<u64> {
        self.applied_krl_version
    }

    /// Parses and merges an OpenSSH binary Key Revocation List (`SSHKRL` / `PROTOCOL.krl`) from
    /// memory, enforcing monotonic `krl_version` anti-rollback. Fails atomically on malformed input.
    pub fn add_krl_bytes(mut self, krl_bytes: &[u8]) -> Result<Self, ModTrustConfigError> {
        let parsed = ParsedKrl::decode(krl_bytes)?;
        let min_required = self
            .min_krl_version
            .max(self.applied_krl_version.unwrap_or(0));
        if parsed.krl_version < min_required {
            return Err(ModTrustConfigError::KrlVersionRollback {
                offered: parsed.krl_version,
                min_required,
            });
        }

        self.revoked_key_fingerprints
            .extend(parsed.revoked_key_fingerprints);
        self.global_cert_revocations.merge(parsed.global_certs);
        for (ca_fp, scope) in parsed.scoped_certs {
            self.ca_cert_revocations
                .entry(ca_fp)
                .or_default()
                .merge(scope);
        }
        self.applied_krl_version = Some(
            self.applied_krl_version
                .map_or(parsed.krl_version, |prev| prev.max(parsed.krl_version)),
        );
        Ok(self)
    }

    /// Reads (bounded by [`MAX_KRL_BYTES`]) and merges a binary OpenSSH `.krl` file from disk.
    pub fn add_krl_file(self, path: impl AsRef<Path>) -> Result<Self, ModTrustConfigError> {
        let bytes = read_bounded_config_bytes(path.as_ref(), MAX_KRL_BYTES)?;
        self.add_krl_bytes(&bytes)
    }

    /// Verifies an external detached `SSHSIG` signature over `krl_bytes` under
    /// [`DEFAULT_KRL_SIGNATURE_NAMESPACE`] using this policy's trust anchors (allowing `trusted_ca_keys`
    /// both as direct KRL signers and as issuing CAs for delegated KRL signing certs), then merges
    /// the KRL with anti-rollback enforcement.
    pub fn add_signed_krl_bytes(
        self,
        krl_bytes: &[u8],
        sig_pem_bytes: &[u8],
    ) -> Result<Self, ModTrustConfigError> {
        if krl_bytes.len() as u64 > MAX_KRL_BYTES {
            return Err(ModTrustConfigError::InvalidRevocationList {
                detail: format!(
                    "KRL size ({} bytes) exceeds limit ({MAX_KRL_BYTES} bytes)",
                    krl_bytes.len()
                ),
            });
        }
        if sig_pem_bytes.len() as u64 > MAX_SIGNATURE_SIDECAR_BYTES {
            return Err(ModTrustConfigError::KrlSignatureInvalid {
                detail: format!(
                    "KRL signature size ({} bytes) exceeds limit ({MAX_SIGNATURE_SIDECAR_BYTES} bytes)",
                    sig_pem_bytes.len()
                ),
            });
        }

        let mut krl_verifier = self.clone();
        krl_verifier.enforcement = SignatureEnforcement::Required;
        krl_verifier.namespace = DEFAULT_KRL_SIGNATURE_NAMESPACE.to_owned();
        krl_verifier.required_principal = None;
        krl_verifier.bind_manifest_name_to_principal = false;
        krl_verifier.max_binary_bytes = MAX_KRL_BYTES;
        for ca in &self.trusted_ca_keys {
            if !krl_verifier
                .trusted_public_keys
                .iter()
                .any(|pk| pk.key_data() == ca.key_data())
            {
                krl_verifier.trusted_public_keys.push(ca.clone());
            }
        }

        krl_verifier
            .verify_bytes(krl_bytes, sig_pem_bytes)
            .map_err(|e| ModTrustConfigError::KrlSignatureInvalid {
                detail: e.to_string(),
            })?;

        self.add_krl_bytes(krl_bytes)
    }

    /// Reads and verifies an externally signed `.krl` file plus its detached `.sig` sidecar
    /// (`sig_path` defaults to `<krl_path>.sig`), then merges it with anti-rollback checks.
    pub fn add_signed_krl_file(
        self,
        krl_path: impl AsRef<Path>,
        sig_path: Option<&Path>,
    ) -> Result<Self, ModTrustConfigError> {
        let krl_ref = krl_path.as_ref();
        let default_sig;
        let sig_ref = match sig_path {
            Some(p) => p,
            None => {
                default_sig = default_signature_path_for(krl_ref);
                &default_sig
            }
        };
        let krl_bytes = read_bounded_config_bytes(krl_ref, MAX_KRL_BYTES)?;
        let sig_bytes = read_bounded_config_bytes(sig_ref, MAX_SIGNATURE_SIDECAR_BYTES)?;
        self.add_signed_krl_bytes(&krl_bytes, &sig_bytes)
    }

    /// Parses and merges OpenSSH `ssh-keygen -k` text revocation lines (`serial: <n|min-max>`,
    /// `id: <key_id>`, `hash: SHA256:<b64>`, `key: <openssh-pub>`). Fails atomically on errors.
    pub fn add_revocation_spec_str(self, spec: &str) -> Result<Self, ModTrustConfigError> {
        let mut staged = self;
        for (idx, raw_line) in spec.lines().enumerate() {
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(rest) = line.strip_prefix("serial:") {
                let val = rest.trim();
                if let Some((a, b)) = val.split_once('-') {
                    let min = a.trim().parse::<u64>().map_err(|e| {
                        ModTrustConfigError::InvalidRevocationList {
                            detail: format!("line {}: invalid serial range start: {e}", idx + 1),
                        }
                    })?;
                    let max = b.trim().parse::<u64>().map_err(|e| {
                        ModTrustConfigError::InvalidRevocationList {
                            detail: format!("line {}: invalid serial range end: {e}", idx + 1),
                        }
                    })?;
                    staged = staged.revoke_serial_range(min, max)?;
                } else {
                    let serial = val.parse::<u64>().map_err(|e| {
                        ModTrustConfigError::InvalidRevocationList {
                            detail: format!("line {}: invalid serial: {e}", idx + 1),
                        }
                    })?;
                    staged = staged.revoke_serial(serial);
                }
            } else if let Some(rest) = line.strip_prefix("id:") {
                let raw_id = rest.trim();
                let key_id = raw_id
                    .strip_prefix('"')
                    .and_then(|s| s.strip_suffix('"'))
                    .unwrap_or(raw_id);
                if key_id.is_empty() {
                    return Err(ModTrustConfigError::InvalidRevocationList {
                        detail: format!("line {}: empty certificate key_id", idx + 1),
                    });
                }
                staged = staged.revoke_key_id(key_id);
            } else if let Some(rest) = line.strip_prefix("hash:") {
                staged = staged.revoke_fingerprint(rest.trim())?;
            } else if let Some(rest) = line.strip_prefix("key:") {
                staged = staged.revoke_key_openssh(rest.trim())?;
            } else {
                return Err(ModTrustConfigError::InvalidRevocationList {
                    detail: format!(
                        "line {}: unrecognized revocation directive '{line}'",
                        idx + 1
                    ),
                });
            }
        }
        Ok(staged)
    }

    fn is_cert_serial_revoked(&self, ca_fp: &Fingerprint, serial: u64) -> bool {
        self.global_cert_revocations.is_serial_revoked(serial)
            || self
                .ca_cert_revocations
                .get(ca_fp)
                .is_some_and(|scope| scope.is_serial_revoked(serial))
    }

    fn is_cert_key_id_revoked(&self, ca_fp: &Fingerprint, key_id: &str) -> bool {
        self.global_cert_revocations.is_key_id_revoked(key_id)
            || self
                .ca_cert_revocations
                .get(ca_fp)
                .is_some_and(|scope| scope.is_key_id_revoked(key_id))
    }

    /// Returns the configured [`SignatureEnforcement`].
    #[must_use]
    pub const fn enforcement(&self) -> SignatureEnforcement {
        self.enforcement
    }

    /// Returns `true` when [`SignatureEnforcement::Required`] is configured.
    #[must_use]
    pub const fn is_enforced(&self) -> bool {
        matches!(self.enforcement, SignatureEnforcement::Required)
    }

    /// Returns the configured `SSHSIG` namespace.
    #[must_use]
    pub fn namespace(&self) -> &str {
        &self.namespace
    }

    /// Returns the required certificate principal, if configured.
    #[must_use]
    pub fn required_principal(&self) -> Option<&str> {
        self.required_principal.as_deref()
    }

    /// Returns the configured [`CertificateValidityPolicy`].
    #[must_use]
    pub const fn validity_policy(&self) -> CertificateValidityPolicy {
        self.validity_policy
    }

    /// Returns the maximum shared-library byte limit permitted for verification.
    #[must_use]
    pub const fn max_binary_bytes(&self) -> u64 {
        self.max_binary_bytes
    }

    /// Returns `true` when `BamManifest::name` must match one of the certificate's principals.
    #[must_use]
    pub const fn binds_manifest_name_to_principal(&self) -> bool {
        self.bind_manifest_name_to_principal
    }

    /// Verifies `binary_bytes` against `sig_pem_bytes` (`-----BEGIN SSH SIGNATURE-----`) under
    /// this policy's trust anchors, revocations, certificate validity window, and namespace.
    pub fn verify_bytes(
        &self,
        binary_bytes: &[u8],
        sig_pem_bytes: &[u8],
    ) -> Result<VerifiedModSignature, SignatureVerificationError> {
        if self.trusted_ca_keys.is_empty() && self.trusted_public_keys.is_empty() {
            return Err(SignatureVerificationError::NoTrustAnchorsConfigured);
        }

        if binary_bytes.is_empty() {
            return Err(SignatureVerificationError::EmptyBinary);
        }

        let bin_len = binary_bytes.len() as u64;
        if bin_len > self.max_binary_bytes {
            return Err(SignatureVerificationError::BinaryTooLarge {
                size: bin_len,
                limit: self.max_binary_bytes,
            });
        }

        let envelope = RawSshSigEnvelope::decode_pem_checked(sig_pem_bytes)?;
        if envelope.namespace != self.namespace {
            return Err(SignatureVerificationError::NamespaceMismatch {
                expected: self.namespace.clone(),
                actual: envelope.namespace,
            });
        }
        if envelope.signature.algorithm() != Algorithm::Ed25519 {
            return Err(SignatureVerificationError::UnsupportedAlgorithm {
                detail: format!(
                    "signature algorithm '{}' is not supported",
                    envelope.signature.algorithm()
                ),
            });
        }

        let (verify_key, signer_fingerprint, ca_fingerprint, key_id, principals, serial) =
            match &envelope.signer {
                SshSigSigner::Certificate(cert_blob) => {
                    let cert = ParsedCertificate::decode(cert_blob)?;
                    if cert.cert_type != CertType::User {
                        return Err(SignatureVerificationError::InvalidCertificateType {
                            detail: format!(
                                "expected User certificate (1), got {:?}",
                                cert.cert_type
                            ),
                        });
                    }
                    if cert.public_key.algorithm() != Algorithm::Ed25519
                        || cert.signature_key.algorithm() != Algorithm::Ed25519
                    {
                        return Err(SignatureVerificationError::UnsupportedAlgorithm {
                            detail: "certificate keys must use Ed25519".to_owned(),
                        });
                    }
                    validate_strict_ed25519_kd(&cert.signature_key).map_err(|detail| {
                        SignatureVerificationError::WeakOrInvalidPublicKey { detail }
                    })?;
                    validate_strict_ed25519_kd(&cert.public_key).map_err(|detail| {
                        SignatureVerificationError::WeakOrInvalidPublicKey { detail }
                    })?;
                    if !cert.critical_options.is_empty() {
                        return Err(SignatureVerificationError::UnrecognizedCriticalOption {
                            options: cert.critical_options.keys().cloned().collect(),
                        });
                    }

                    let ca_fp = cert.signature_key.fingerprint(HashAlg::Sha256);
                    let signer_fp = cert.public_key.fingerprint(HashAlg::Sha256);

                    if !self
                        .trusted_ca_keys
                        .iter()
                        .any(|ca| ca.key_data() == &cert.signature_key)
                    {
                        return Err(SignatureVerificationError::UntrustedCertificateAuthority {
                            ca_fingerprint: ca_fp.to_string(),
                        });
                    }

                    verify_strict_ed25519_sig(&cert.signature_key, cert.tbs_bytes, &cert.signature)
                        .map_err(|err| match err {
                            StrictSigVerifyError::WeakOrInvalidKey(detail) => {
                                SignatureVerificationError::WeakOrInvalidPublicKey { detail }
                            }
                            StrictSigVerifyError::InvalidSignature => {
                                SignatureVerificationError::UntrustedCertificateAuthority {
                                    ca_fingerprint: ca_fp.to_string(),
                                }
                            }
                        })?;

                    if self.revoked_key_fingerprints.contains(&ca_fp) {
                        return Err(SignatureVerificationError::SignerRevoked {
                            fingerprint: ca_fp.to_string(),
                        });
                    }
                    if self.revoked_key_fingerprints.contains(&signer_fp) {
                        return Err(SignatureVerificationError::SignerRevoked {
                            fingerprint: signer_fp.to_string(),
                        });
                    }
                    if self.is_cert_serial_revoked(&ca_fp, cert.serial) {
                        return Err(SignatureVerificationError::CertificateSerialRevoked {
                            serial: cert.serial,
                        });
                    }
                    if self.is_cert_key_id_revoked(&ca_fp, &cert.key_id) {
                        return Err(SignatureVerificationError::CertificateKeyIdRevoked {
                            key_id: cert.key_id,
                        });
                    }

                    validate_cert_time_window(
                        cert.valid_after,
                        cert.valid_before,
                        self.validity_policy,
                    )?;

                    if let Some(expected) = &self.required_principal
                        && !cert.valid_principals.iter().any(|p| p == expected)
                    {
                        return Err(SignatureVerificationError::PrincipalMismatch {
                            expected: expected.clone(),
                            actual: cert.valid_principals,
                        });
                    }

                    (
                        cert.public_key,
                        signer_fp.to_string(),
                        Some(ca_fp.to_string()),
                        Some(cert.key_id),
                        cert.valid_principals,
                        Some(cert.serial),
                    )
                }
                SshSigSigner::PublicKey(kd) => {
                    if kd.algorithm() != Algorithm::Ed25519 {
                        return Err(SignatureVerificationError::UnsupportedAlgorithm {
                            detail: format!(
                                "public key algorithm '{}' is not supported",
                                kd.algorithm()
                            ),
                        });
                    }
                    validate_strict_ed25519_kd(kd).map_err(|detail| {
                        SignatureVerificationError::WeakOrInvalidPublicKey { detail }
                    })?;
                    let signer_fp = kd.fingerprint(HashAlg::Sha256);
                    if self.revoked_key_fingerprints.contains(&signer_fp) {
                        return Err(SignatureVerificationError::SignerRevoked {
                            fingerprint: signer_fp.to_string(),
                        });
                    }
                    if !self
                        .trusted_public_keys
                        .iter()
                        .any(|pk| pk.key_data() == kd)
                    {
                        return Err(SignatureVerificationError::UntrustedPublicKey {
                            fingerprint: signer_fp.to_string(),
                        });
                    }
                    if let Some(expected) = &self.required_principal {
                        return Err(SignatureVerificationError::PrincipalMismatch {
                            expected: expected.clone(),
                            actual: Vec::new(),
                        });
                    }

                    (
                        kd.clone(),
                        signer_fp.to_string(),
                        None,
                        None,
                        Vec::new(),
                        None,
                    )
                }
            };

        let digest = envelope.hash_alg.digest(binary_bytes);
        let signed_data = encode_signed_data(
            &envelope.namespace,
            &envelope.reserved,
            envelope.hash_alg,
            &digest,
        )
        .map_err(|e| SignatureVerificationError::MalformedSignature {
            detail: e.to_string(),
        })?;

        verify_strict_ed25519_sig(&verify_key, &signed_data, &envelope.signature).map_err(
            |err| match err {
                StrictSigVerifyError::WeakOrInvalidKey(detail) => {
                    SignatureVerificationError::WeakOrInvalidPublicKey { detail }
                }
                StrictSigVerifyError::InvalidSignature => {
                    SignatureVerificationError::InvalidSignature
                }
            },
        )?;

        let binary_sha256: [u8; 32] = Sha256::digest(binary_bytes).into();
        Ok(VerifiedModSignature {
            signer_fingerprint,
            ca_fingerprint,
            key_id,
            principals,
            serial,
            namespace: envelope.namespace,
            binary_sha256,
        })
    }

    /// Re-validates an already-issued [`VerifiedModSignature`] receipt against this policy's
    /// active trust anchors, KRLs/revocations, `required_principal`, and `namespace`.
    pub fn verify_receipt_still_trusted(
        &self,
        receipt: &VerifiedModSignature,
    ) -> Result<(), SignatureVerificationError> {
        if !self.is_enforced() {
            return Ok(());
        }
        if self.trusted_ca_keys.is_empty() && self.trusted_public_keys.is_empty() {
            return Err(SignatureVerificationError::NoTrustAnchorsConfigured);
        }
        if receipt.namespace != self.namespace {
            return Err(SignatureVerificationError::NamespaceMismatch {
                expected: self.namespace.clone(),
                actual: receipt.namespace.clone(),
            });
        }

        let signer_fp: Fingerprint =
            receipt
                .signer_fingerprint
                .parse()
                .map_err(
                    |e: ssh_key::Error| SignatureVerificationError::MalformedSignature {
                        detail: format!(
                            "invalid signer fingerprint '{}': {e}",
                            receipt.signer_fingerprint
                        ),
                    },
                )?;
        if self.revoked_key_fingerprints.contains(&signer_fp) {
            return Err(SignatureVerificationError::SignerRevoked {
                fingerprint: receipt.signer_fingerprint.clone(),
            });
        }

        if let Some(ca_fp_str) = &receipt.ca_fingerprint {
            let ca_fp: Fingerprint = ca_fp_str.parse().map_err(|e: ssh_key::Error| {
                SignatureVerificationError::MalformedSignature {
                    detail: format!("invalid CA fingerprint '{ca_fp_str}': {e}"),
                }
            })?;
            if !self
                .trusted_ca_keys
                .iter()
                .any(|ca| ca.fingerprint(HashAlg::Sha256) == ca_fp)
            {
                return Err(SignatureVerificationError::UntrustedCertificateAuthority {
                    ca_fingerprint: ca_fp_str.clone(),
                });
            }
            if self.revoked_key_fingerprints.contains(&ca_fp) {
                return Err(SignatureVerificationError::SignerRevoked {
                    fingerprint: ca_fp_str.clone(),
                });
            }
            if let Some(serial) = receipt.serial
                && self.is_cert_serial_revoked(&ca_fp, serial)
            {
                return Err(SignatureVerificationError::CertificateSerialRevoked { serial });
            }
            if let Some(key_id) = &receipt.key_id
                && self.is_cert_key_id_revoked(&ca_fp, key_id)
            {
                return Err(SignatureVerificationError::CertificateKeyIdRevoked {
                    key_id: key_id.clone(),
                });
            }
            if let Some(expected) = &self.required_principal
                && !receipt.principals.iter().any(|p| p == expected)
            {
                return Err(SignatureVerificationError::PrincipalMismatch {
                    expected: expected.clone(),
                    actual: receipt.principals.clone(),
                });
            }
        } else {
            if !self
                .trusted_public_keys
                .iter()
                .any(|pk| pk.fingerprint(HashAlg::Sha256) == signer_fp)
            {
                return Err(SignatureVerificationError::UntrustedPublicKey {
                    fingerprint: receipt.signer_fingerprint.clone(),
                });
            }
            if let Some(expected) = &self.required_principal {
                return Err(SignatureVerificationError::PrincipalMismatch {
                    expected: expected.clone(),
                    actual: Vec::new(),
                });
            }
        }
        Ok(())
    }

    /// Checks [`VerifiedModSignature::principals`] against `manifest_name` when
    /// `bind_manifest_name_to_principal` is enabled.
    pub fn verify_manifest_principal(
        &self,
        receipt: &VerifiedModSignature,
        manifest_name: &str,
    ) -> Result<(), SignatureVerificationError> {
        if self.bind_manifest_name_to_principal
            && !receipt.principals.iter().any(|p| p == manifest_name)
        {
            return Err(SignatureVerificationError::PrincipalMismatch {
                expected: manifest_name.to_owned(),
                actual: receipt.principals.clone(),
            });
        }
        Ok(())
    }
}

fn validate_cert_time_window(
    valid_after: u64,
    valid_before: u64,
    policy: CertificateValidityPolicy,
) -> Result<(), SignatureVerificationError> {
    if valid_after >= valid_before {
        let check_time = match policy {
            CertificateValidityPolicy::SystemClock => current_unix_secs()?,
            CertificateValidityPolicy::FixedTimestamp(ts) => ts,
            CertificateValidityPolicy::IgnoreTimestamps => {
                return Err(SignatureVerificationError::MalformedSignature {
                    detail: format!(
                        "certificate validity window is empty/inverted ({valid_after} >= {valid_before})"
                    ),
                });
            }
        };
        return Err(
            SignatureVerificationError::CertificateExpiredOrNotYetValid {
                valid_after,
                valid_before,
                check_time,
            },
        );
    }

    let check_time = match policy {
        CertificateValidityPolicy::SystemClock => current_unix_secs()?,
        CertificateValidityPolicy::FixedTimestamp(ts) => ts,
        CertificateValidityPolicy::IgnoreTimestamps => return Ok(()),
    };

    if valid_after <= check_time && check_time < valid_before {
        Ok(())
    } else {
        Err(
            SignatureVerificationError::CertificateExpiredOrNotYetValid {
                valid_after,
                valid_before,
                check_time,
            },
        )
    }
}

fn current_unix_secs() -> Result<u64, SignatureVerificationError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| SignatureVerificationError::MalformedSignature {
            detail: "system clock precedes UNIX_EPOCH".to_owned(),
        })
}

/// Signs `binary_bytes` using a raw Ed25519 [`PrivateKey`] and produces an OpenSSH
/// `-----BEGIN SSH SIGNATURE-----` PEM string.
pub fn sign_mod_bytes_with_key(
    signing_key: &PrivateKey,
    namespace: &str,
    binary_bytes: &[u8],
) -> Result<String, ModSigningError> {
    if namespace.is_empty() {
        return Err(ModSigningError::EmptyNamespace);
    }
    if signing_key.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: signing_key.algorithm().to_string(),
        });
    }
    let signer = SshSigSigner::PublicKey(signing_key.public_key().key_data().clone());
    sign_mod_bytes_inner(signing_key, signer, namespace, binary_bytes)
}

/// Signs `binary_bytes` using an Ed25519 [`PrivateKey`] and an issued [`Certificate`],
/// embedding the certificate inside the resulting `-----BEGIN SSH SIGNATURE-----` PEM string.
pub fn sign_mod_bytes_with_certificate(
    signing_key: &PrivateKey,
    certificate: &Certificate,
    namespace: &str,
    binary_bytes: &[u8],
) -> Result<String, ModSigningError> {
    if namespace.is_empty() {
        return Err(ModSigningError::EmptyNamespace);
    }
    if signing_key.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: signing_key.algorithm().to_string(),
        });
    }
    if certificate.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: certificate.algorithm().to_string(),
        });
    }
    if signing_key.public_key().key_data() != certificate.public_key() {
        return Err(ModSigningError::MismatchedCertificatePublicKey);
    }
    let cert_bytes = certificate
        .to_bytes()
        .map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?;
    sign_mod_bytes_inner(
        signing_key,
        SshSigSigner::Certificate(cert_bytes),
        namespace,
        binary_bytes,
    )
}

/// Signs `binary_bytes` using an Ed25519 [`PrivateKey`] and an OpenSSH-formatted certificate
/// line (for example, `*-cert.pub` generated by `ssh-keygen -s`, including `"forever"`
/// `valid_before == u64::MAX` certificates).
pub fn sign_mod_bytes_with_openssh_certificate(
    signing_key: &PrivateKey,
    certificate_openssh: &str,
    namespace: &str,
    binary_bytes: &[u8],
) -> Result<String, ModSigningError> {
    if namespace.is_empty() {
        return Err(ModSigningError::EmptyNamespace);
    }
    if signing_key.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: signing_key.algorithm().to_string(),
        });
    }
    let cert_bytes = decode_openssh_cert_line(certificate_openssh)?;
    let parsed = ParsedCertificate::decode(&cert_bytes).map_err(|e| ModSigningError::Encoding {
        detail: e.to_string(),
    })?;
    if &parsed.public_key != signing_key.public_key().key_data() {
        return Err(ModSigningError::MismatchedCertificatePublicKey);
    }
    sign_mod_bytes_inner(
        signing_key,
        SshSigSigner::Certificate(cert_bytes),
        namespace,
        binary_bytes,
    )
}

fn sign_mod_bytes_inner(
    signing_key: &PrivateKey,
    signer: SshSigSigner,
    namespace: &str,
    binary_bytes: &[u8],
) -> Result<String, ModSigningError> {
    let hash_alg = HashAlg::Sha512;
    let digest = hash_alg.digest(binary_bytes);
    let signed_data = encode_signed_data(namespace, &[], hash_alg, &digest).map_err(|e| {
        ModSigningError::Encoding {
            detail: e.to_string(),
        }
    })?;
    let signature = signing_key
        .try_sign(&signed_data)
        .map_err(|e| ModSigningError::Signing {
            detail: e.to_string(),
        })?;

    let envelope = RawSshSigEnvelope {
        version: SSHSIG_SUPPORTED_VERSION,
        signer,
        namespace: namespace.to_owned(),
        reserved: Vec::new(),
        hash_alg,
        signature,
    };

    envelope
        .encode_pem_string(LineEnding::LF)
        .map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })
}

fn decode_openssh_cert_line(line: &str) -> Result<Vec<u8>, ModSigningError> {
    let mut parts = line.split_whitespace();
    let Some(alg_id) = parts.next() else {
        return Err(ModSigningError::Encoding {
            detail: "empty OpenSSH certificate string".to_owned(),
        });
    };
    if alg_id != ED25519_CERT_ALGORITHM_ID {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: alg_id.to_owned(),
        });
    }
    let Some(b64) = parts.next() else {
        return Err(ModSigningError::Encoding {
            detail: "missing base64 certificate payload".to_owned(),
        });
    };
    let mut reader =
        ssh_encoding::Base64Reader::new(b64.as_bytes()).map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?;
    let mut out = vec![0u8; reader.remaining_len()];
    reader
        .read(&mut out)
        .map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?;
    reader.finish(()).map_err(|e| ModSigningError::Encoding {
        detail: e.to_string(),
    })?;
    Ok(out)
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum SshSigSigner {
    PublicKey(KeyData),
    Certificate(Vec<u8>),
}

impl SshSigSigner {
    fn encoded_len_inner(&self) -> ssh_encoding::Result<usize> {
        match self {
            Self::PublicKey(kd) => kd.encoded_len(),
            Self::Certificate(blob) => Ok(blob.len()),
        }
    }

    fn encode_inner(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        match self {
            Self::PublicKey(kd) => kd.encode(writer),
            Self::Certificate(blob) => writer.write(blob),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawSshSigEnvelope {
    version: u32,
    signer: SshSigSigner,
    namespace: String,
    reserved: Vec<u8>,
    hash_alg: HashAlg,
    signature: Signature,
}

impl RawSshSigEnvelope {
    fn decode_pem_checked(pem: &[u8]) -> Result<Self, SignatureVerificationError> {
        let envelope = Self::decode_pem(pem).map_err(|err| match err {
            EnvelopeError::UnsupportedAlgorithm(detail) => {
                SignatureVerificationError::UnsupportedAlgorithm { detail }
            }
            EnvelopeError::Malformed(detail) => {
                SignatureVerificationError::MalformedSignature { detail }
            }
        })?;
        Ok(envelope)
    }
}

#[derive(Debug)]
enum EnvelopeError {
    Malformed(String),
    UnsupportedAlgorithm(String),
}

impl From<ssh_encoding::Error> for EnvelopeError {
    fn from(err: ssh_encoding::Error) -> Self {
        Self::Malformed(err.to_string())
    }
}

impl From<ssh_key::Error> for EnvelopeError {
    fn from(err: ssh_key::Error) -> Self {
        Self::Malformed(err.to_string())
    }
}

impl PemLabel for RawSshSigEnvelope {
    const PEM_LABEL: &'static str = "SSH SIGNATURE";
}

impl Decode for RawSshSigEnvelope {
    type Error = EnvelopeError;

    fn decode(reader: &mut impl Reader) -> Result<Self, Self::Error> {
        let mut magic = [0u8; SSHSIG_MAGIC_PREAMBLE.len()];
        reader.read(&mut magic)?;
        if magic != SSHSIG_MAGIC_PREAMBLE {
            return Err(EnvelopeError::Malformed(
                "missing SSHSIG magic preamble".to_owned(),
            ));
        }

        let version = u32::decode(reader)?;
        if version != SSHSIG_SUPPORTED_VERSION {
            return Err(EnvelopeError::Malformed(format!(
                "unsupported SSHSIG version {version}"
            )));
        }

        let pk_blob = Vec::<u8>::decode(reader)?;
        let mut peek = pk_blob.as_slice();
        let alg = String::decode(&mut peek)?;
        let signer = if alg.ends_with("-cert-v01@openssh.com") {
            if alg != ED25519_CERT_ALGORITHM_ID {
                return Err(EnvelopeError::UnsupportedAlgorithm(alg));
            }
            SshSigSigner::Certificate(pk_blob)
        } else {
            let mut pk_reader = pk_blob.as_slice();
            let kd = KeyData::decode(&mut pk_reader)?;
            pk_reader.finish(())?;
            if kd.algorithm() != Algorithm::Ed25519 {
                return Err(EnvelopeError::UnsupportedAlgorithm(
                    kd.algorithm().to_string(),
                ));
            }
            SshSigSigner::PublicKey(kd)
        };

        let namespace = String::decode(reader)?;
        if namespace.is_empty() {
            return Err(EnvelopeError::Malformed(
                "SSHSIG namespace must not be empty".to_owned(),
            ));
        }

        let reserved = Vec::<u8>::decode(reader)?;
        let hash_alg = HashAlg::decode(reader)?;
        let signature = reader.read_prefixed(|r| {
            let sig = Signature::decode(r)?;
            if !r.is_finished() {
                return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                    remaining: r.remaining_len(),
                }));
            }
            Ok(sig)
        })?;

        Ok(Self {
            version,
            signer,
            namespace,
            reserved,
            hash_alg,
            signature,
        })
    }
}

impl Encode for RawSshSigEnvelope {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        let signer_inner = self.signer.encoded_len_inner()?;
        [
            SSHSIG_MAGIC_PREAMBLE.len(),
            self.version.encoded_len()?,
            4,
            signer_inner,
            self.namespace.encoded_len()?,
            self.reserved.encoded_len()?,
            self.hash_alg.encoded_len()?,
            self.signature.encoded_len_prefixed()?,
        ]
        .checked_sum()
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(SSHSIG_MAGIC_PREAMBLE)?;
        self.version.encode(writer)?;
        self.signer.encoded_len_inner()?.encode(writer)?;
        self.signer.encode_inner(writer)?;
        self.namespace.encode(writer)?;
        self.reserved.encode(writer)?;
        self.hash_alg.encode(writer)?;
        self.signature.encode_prefixed(writer)?;
        Ok(())
    }
}

struct ParsedCertificate<'a> {
    public_key: KeyData,
    serial: u64,
    cert_type: CertType,
    key_id: String,
    valid_principals: Vec<String>,
    valid_after: u64,
    valid_before: u64,
    critical_options: OptionsMap,
    signature_key: KeyData,
    signature: Signature,
    tbs_bytes: &'a [u8],
}

impl<'a> ParsedCertificate<'a> {
    fn decode(cert_bytes: &'a [u8]) -> Result<Self, SignatureVerificationError> {
        let mut reader = cert_bytes;
        let map_malformed =
            |msg: String| SignatureVerificationError::MalformedSignature { detail: msg };

        let cert_alg = String::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        if cert_alg != ED25519_CERT_ALGORITHM_ID {
            return Err(SignatureVerificationError::UnsupportedAlgorithm { detail: cert_alg });
        }

        let nonce = Vec::<u8>::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        if nonce.is_empty() {
            return Err(SignatureVerificationError::MalformedSignature {
                detail: "certificate nonce must not be empty".to_owned(),
            });
        }
        let ed_pk =
            Ed25519PublicKey::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let public_key = KeyData::Ed25519(ed_pk);
        let serial = u64::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let cert_type = CertType::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let key_id = String::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let valid_principals =
            Vec::<String>::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let valid_after = u64::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let valid_before = u64::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let critical_options =
            OptionsMap::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let _extensions =
            OptionsMap::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let _reserved = Vec::<u8>::decode(&mut reader).map_err(|e| map_malformed(e.to_string()))?;
        let signature_key = reader
            .read_prefixed(|r| {
                let key = KeyData::decode(r)?;
                if !r.is_finished() {
                    return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                        remaining: r.remaining_len(),
                    }));
                }
                Ok(key)
            })
            .map_err(|e| map_malformed(e.to_string()))?;

        let tbs_len = cert_bytes.len().saturating_sub(reader.remaining_len());
        let tbs_bytes = &cert_bytes[..tbs_len];

        let signature = reader
            .read_prefixed(|r| {
                let sig = Signature::decode(r)?;
                if !r.is_finished() {
                    return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                        remaining: r.remaining_len(),
                    }));
                }
                Ok(sig)
            })
            .map_err(|e| map_malformed(e.to_string()))?;

        reader
            .finish(())
            .map_err(|e| map_malformed(e.to_string()))?;

        Ok(Self {
            public_key,
            serial,
            cert_type,
            key_id,
            valid_principals,
            valid_after,
            valid_before,
            critical_options,
            signature_key,
            signature,
            tbs_bytes,
        })
    }
}

struct SignedData<'a> {
    namespace: &'a str,
    reserved: &'a [u8],
    hash_alg: HashAlg,
    hash: &'a [u8],
}

impl Encode for SignedData<'_> {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        [
            SSHSIG_MAGIC_PREAMBLE.len(),
            self.namespace.encoded_len()?,
            self.reserved.encoded_len()?,
            self.hash_alg.encoded_len()?,
            self.hash.encoded_len()?,
        ]
        .checked_sum()
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(SSHSIG_MAGIC_PREAMBLE)?;
        self.namespace.encode(writer)?;
        self.reserved.encode(writer)?;
        self.hash_alg.encode(writer)?;
        self.hash.encode(writer)?;
        Ok(())
    }
}

fn encode_signed_data(
    namespace: &str,
    reserved: &[u8],
    hash_alg: HashAlg,
    hash: &[u8],
) -> ssh_encoding::Result<Vec<u8>> {
    let data = SignedData {
        namespace,
        reserved,
        hash_alg,
        hash,
    };
    let mut out = Vec::with_capacity(data.encoded_len()?);
    data.encode(&mut out)?;
    Ok(out)
}

struct ParsedKrl {
    krl_version: u64,
    revoked_key_fingerprints: BTreeSet<Fingerprint>,
    global_certs: CertRevocationScope,
    scoped_certs: BTreeMap<Fingerprint, CertRevocationScope>,
}

impl ParsedKrl {
    fn decode(krl_bytes: &[u8]) -> Result<Self, ModTrustConfigError> {
        if krl_bytes.len() as u64 > MAX_KRL_BYTES {
            return Err(ModTrustConfigError::InvalidRevocationList {
                detail: format!(
                    "KRL size ({} bytes) exceeds maximum ({MAX_KRL_BYTES} bytes)",
                    krl_bytes.len()
                ),
            });
        }
        let map_err = |msg: String| ModTrustConfigError::InvalidRevocationList { detail: msg };

        let mut reader = krl_bytes;
        let mut magic = [0u8; SSHKRL_MAGIC.len()];
        reader
            .read(&mut magic)
            .map_err(|e| map_err(e.to_string()))?;
        if magic != *SSHKRL_MAGIC {
            return Err(map_err("missing SSHKRL magic header".to_owned()));
        }

        let format_version = u32::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
        if format_version != SSHKRL_SUPPORTED_FORMAT_VERSION {
            return Err(map_err(format!(
                "unsupported KRL format version {format_version}"
            )));
        }

        let krl_version = u64::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
        let _generated_date = u64::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
        let flags = u64::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
        if flags != 0 {
            return Err(map_err(format!("unsupported non-zero KRL flags {flags}")));
        }
        let _reserved = Vec::<u8>::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
        let _comment = Vec::<u8>::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;

        let mut revoked_key_fingerprints = BTreeSet::new();
        let mut global_certs = CertRevocationScope::default();
        let mut scoped_certs: BTreeMap<Fingerprint, CertRevocationScope> = BTreeMap::new();

        while !reader.is_finished() {
            let section_type = u8::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
            let section_data =
                Vec::<u8>::decode(&mut reader).map_err(|e| map_err(e.to_string()))?;
            let mut sec_reader = section_data.as_slice();

            match section_type {
                KRL_SECTION_CERTIFICATES => {
                    let ca_blob =
                        Vec::<u8>::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;
                    let ca_fp = if ca_blob.is_empty() {
                        None
                    } else {
                        let mut ca_r = ca_blob.as_slice();
                        let kd = KeyData::decode(&mut ca_r).map_err(|e| map_err(e.to_string()))?;
                        ca_r.finish(()).map_err(|e| map_err(e.to_string()))?;
                        Some(kd.fingerprint(HashAlg::Sha256))
                    };
                    let _cert_reserved =
                        Vec::<u8>::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;

                    let mut scope = CertRevocationScope::default();
                    while !sec_reader.is_finished() {
                        let sub_type =
                            u8::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;
                        let sub_data = Vec::<u8>::decode(&mut sec_reader)
                            .map_err(|e| map_err(e.to_string()))?;
                        let mut sub_reader = sub_data.as_slice();

                        match sub_type {
                            KRL_SECTION_CERT_SERIAL_LIST => {
                                while !sub_reader.is_finished() {
                                    let s = u64::decode(&mut sub_reader)
                                        .map_err(|e| map_err(e.to_string()))?;
                                    scope.serials.insert(s);
                                }
                            }
                            KRL_SECTION_CERT_SERIAL_RANGE => {
                                let min = u64::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                let max = u64::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                if min > max {
                                    return Err(map_err(format!(
                                        "inverted KRL serial range: {min} > {max}"
                                    )));
                                }
                                scope.serial_ranges.push((min, max));
                            }
                            KRL_SECTION_CERT_SERIAL_BITMAP => {
                                let offset = u64::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                let mpint = Vec::<u8>::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                if !mpint.is_empty() && (mpint[0] & 0x80) != 0 {
                                    return Err(map_err(
                                        "negative mpint serial bitmap in KRL".to_owned(),
                                    ));
                                }
                                scope.serial_bitmaps.push(SerialBitmap {
                                    offset,
                                    big_endian_bytes: mpint,
                                });
                            }
                            KRL_SECTION_CERT_KEY_ID => {
                                while !sub_reader.is_finished() {
                                    let key_id = String::decode(&mut sub_reader)
                                        .map_err(|e| map_err(e.to_string()))?;
                                    scope.key_ids.insert(key_id);
                                }
                            }
                            KRL_SECTION_CERT_EXTENSION => {
                                let ext_name = String::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                let is_critical = u8::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                let _ext_val = Vec::<u8>::decode(&mut sub_reader)
                                    .map_err(|e| map_err(e.to_string()))?;
                                if is_critical != 0 {
                                    return Err(map_err(format!(
                                        "unrecognized critical KRL certificate extension '{ext_name}'"
                                    )));
                                }
                            }
                            other => {
                                return Err(map_err(format!(
                                    "unknown KRL certificate sub-section type 0x{other:02x}"
                                )));
                            }
                        }
                        sub_reader.finish(()).map_err(|e| map_err(e.to_string()))?;
                    }
                    match ca_fp {
                        Some(fp) => scoped_certs.entry(fp).or_default().merge(scope),
                        None => global_certs.merge(scope),
                    }
                }
                KRL_SECTION_EXPLICIT_KEY => {
                    while !sec_reader.is_finished() {
                        let key_blob = Vec::<u8>::decode(&mut sec_reader)
                            .map_err(|e| map_err(e.to_string()))?;
                        let mut kr = key_blob.as_slice();
                        let kd = KeyData::decode(&mut kr).map_err(|e| map_err(e.to_string()))?;
                        kr.finish(()).map_err(|e| map_err(e.to_string()))?;
                        revoked_key_fingerprints.insert(kd.fingerprint(HashAlg::Sha256));
                    }
                }
                KRL_SECTION_FINGERPRINT_SHA256 => {
                    while !sec_reader.is_finished() {
                        let digest = Vec::<u8>::decode(&mut sec_reader)
                            .map_err(|e| map_err(e.to_string()))?;
                        let arr: [u8; 32] = digest.try_into().map_err(|v: Vec<u8>| {
                            map_err(format!(
                                "invalid SHA-256 fingerprint length {} (expected 32)",
                                v.len()
                            ))
                        })?;
                        revoked_key_fingerprints.insert(Fingerprint::Sha256(arr));
                    }
                }
                KRL_SECTION_FINGERPRINT_SHA1 => {
                    return Err(map_err(
                        "deprecated SHA-1 fingerprint KRL section (0x03) is rejected".to_owned(),
                    ));
                }
                KRL_SECTION_SIGNATURE => {
                    return Err(map_err(
                        "deprecated inline KRL signature section (0x04) is rejected; use detached SSHSIG signed KRLs"
                            .to_owned(),
                    ));
                }
                KRL_SECTION_EXTENSION => {
                    let ext_name =
                        String::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;
                    let is_critical =
                        u8::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;
                    let _ext_val =
                        Vec::<u8>::decode(&mut sec_reader).map_err(|e| map_err(e.to_string()))?;
                    if is_critical != 0 {
                        return Err(map_err(format!(
                            "unrecognized critical KRL extension '{ext_name}'"
                        )));
                    }
                }
                other => {
                    return Err(map_err(format!("unknown KRL section type 0x{other:02x}")));
                }
            }

            sec_reader.finish(()).map_err(|e| map_err(e.to_string()))?;
        }

        Ok(Self {
            krl_version,
            revoked_key_fingerprints,
            global_certs,
            scoped_certs,
        })
    }
}

#[derive(Clone, Debug, Default)]
struct KrlCertBuilderScope {
    serials: Vec<u64>,
    ranges: Vec<(u64, u64)>,
    bitmaps: Vec<(u64, Vec<u64>)>,
    key_ids: Vec<String>,
}

impl KrlCertBuilderScope {
    fn is_empty(&self) -> bool {
        self.serials.is_empty()
            && self.ranges.is_empty()
            && self.bitmaps.is_empty()
            && self.key_ids.is_empty()
    }

    fn encode_section(
        &self,
        ca: Option<&KeyData>,
        out: &mut Vec<u8>,
    ) -> Result<(), ModSigningError> {
        let map_enc = |e: ssh_encoding::Error| ModSigningError::Encoding {
            detail: e.to_string(),
        };
        let mut sec = Vec::new();
        match ca {
            None => ([] as [u8; 0])
                .as_slice()
                .encode(&mut sec)
                .map_err(map_enc)?,
            Some(kd) => {
                let mut kd_buf = Vec::new();
                kd.encode(&mut kd_buf).map_err(map_enc)?;
                kd_buf.encode(&mut sec).map_err(map_enc)?;
            }
        }
        ([] as [u8; 0])
            .as_slice()
            .encode(&mut sec)
            .map_err(map_enc)?;

        if !self.serials.is_empty() {
            KRL_SECTION_CERT_SERIAL_LIST
                .encode(&mut sec)
                .map_err(map_enc)?;
            let mut sub = Vec::with_capacity(self.serials.len() * 8);
            for s in &self.serials {
                s.encode(&mut sub).map_err(map_enc)?;
            }
            sub.encode(&mut sec).map_err(map_enc)?;
        }

        for &(min, max) in &self.ranges {
            KRL_SECTION_CERT_SERIAL_RANGE
                .encode(&mut sec)
                .map_err(map_enc)?;
            let mut sub = Vec::with_capacity(16);
            min.encode(&mut sub).map_err(map_enc)?;
            max.encode(&mut sub).map_err(map_enc)?;
            sub.encode(&mut sec).map_err(map_enc)?;
        }

        for (offset, serials) in &self.bitmaps {
            KRL_SECTION_CERT_SERIAL_BITMAP
                .encode(&mut sec)
                .map_err(map_enc)?;
            let mut sub = Vec::new();
            offset.encode(&mut sub).map_err(map_enc)?;
            let mpint = encode_serial_bitmap_mpint(*offset, serials)?;
            mpint.encode(&mut sub).map_err(map_enc)?;
            sub.encode(&mut sec).map_err(map_enc)?;
        }

        if !self.key_ids.is_empty() {
            KRL_SECTION_CERT_KEY_ID.encode(&mut sec).map_err(map_enc)?;
            let mut sub = Vec::new();
            for kid in &self.key_ids {
                kid.encode(&mut sub).map_err(map_enc)?;
            }
            sub.encode(&mut sec).map_err(map_enc)?;
        }

        KRL_SECTION_CERTIFICATES.encode(out).map_err(map_enc)?;
        sec.encode(out).map_err(map_enc)?;
        Ok(())
    }
}

fn encode_serial_bitmap_mpint(offset: u64, serials: &[u64]) -> Result<Vec<u8>, ModSigningError> {
    if serials.is_empty() {
        return Ok(Vec::new());
    }
    let mut max_delta = 0u64;
    for &s in serials {
        let Some(delta) = s.checked_sub(offset) else {
            return Err(ModSigningError::Encoding {
                detail: format!("bitmap serial {s} precedes bitmap offset {offset}"),
            });
        };
        if delta > 8 * 1024 * 1024 {
            return Err(ModSigningError::Encoding {
                detail: format!("bitmap delta {delta} exceeds maximum bitmap span (1 MiB)"),
            });
        }
        max_delta = max_delta.max(delta);
    }
    let byte_len = (max_delta as usize / 8) + 1;
    let mut be = vec![0u8; byte_len];
    for &s in serials {
        let delta = (s - offset) as usize;
        let byte_from_lsb = delta / 8;
        let bit = (delta % 8) as u8;
        let idx = byte_len - 1 - byte_from_lsb;
        be[idx] |= 1u8 << bit;
    }
    while be.first() == Some(&0) {
        be.remove(0);
    }
    if !be.is_empty() && (be[0] & 0x80) != 0 {
        be.insert(0, 0);
    }
    Ok(be)
}

/// Pure-Rust builder for binary OpenSSH Key Revocation Lists (`SSHKRL` / `PROTOCOL.krl`),
/// interoperable with `ssh-keygen -Q` and [`ModTrustPolicy::add_krl_bytes`].
#[derive(Clone, Debug, Default)]
pub struct KrlBuilder {
    krl_version: u64,
    generated_date: u64,
    comment: String,
    explicit_keys: Vec<KeyData>,
    sha256_fingerprints: Vec<[u8; 32]>,
    global_scope: KrlCertBuilderScope,
    ca_scopes: Vec<(KeyData, KrlCertBuilderScope)>,
}

impl KrlBuilder {
    /// Creates an empty OpenSSH KRL builder (`krl_version = 0`, `generated_date = 0`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the monotonic 64-bit KRL version stored in the `SSHKRL` header.
    #[must_use]
    pub const fn version(mut self, krl_version: u64) -> Self {
        self.krl_version = krl_version;
        self
    }

    /// Sets the `generated_date` Unix timestamp in the `SSHKRL` header.
    #[must_use]
    pub const fn generated_date(mut self, unix_secs: u64) -> Self {
        self.generated_date = unix_secs;
        self
    }

    /// Sets the comment string embedded in the `SSHKRL` header.
    #[must_use]
    pub fn comment(mut self, comment: impl Into<String>) -> Self {
        self.comment = comment.into();
        self
    }

    fn scope_for_mut(&mut self, ca: Option<&PublicKey>) -> &mut KrlCertBuilderScope {
        let Some(ca_pk) = ca else {
            return &mut self.global_scope;
        };
        let ca_kd = ca_pk.key_data();
        if let Some(pos) = self.ca_scopes.iter().position(|(kd, _)| kd == ca_kd) {
            &mut self.ca_scopes[pos].1
        } else {
            self.ca_scopes
                .push((ca_kd.clone(), KrlCertBuilderScope::default()));
            let last = self.ca_scopes.len() - 1;
            &mut self.ca_scopes[last].1
        }
    }

    /// Adds an explicit OpenSSH Ed25519 public key revocation (`KRL_SECTION_EXPLICIT_KEY`).
    pub fn revoke_key_openssh(mut self, openssh_pub: &str) -> Result<Self, ModTrustConfigError> {
        let key = parse_ed25519_openssh_key(openssh_pub)?;
        self.explicit_keys.push(key.key_data().clone());
        Ok(self)
    }

    /// Adds an explicit SHA-256 key fingerprint revocation (`KRL_SECTION_FINGERPRINT_SHA256`).
    pub fn revoke_fingerprint(mut self, fingerprint: &str) -> Result<Self, ModTrustConfigError> {
        let trimmed = fingerprint.trim();
        let fp: Fingerprint = trimmed.parse().map_err(|e: ssh_key::Error| {
            ModTrustConfigError::InvalidFingerprint {
                fingerprint: trimmed.to_owned(),
                detail: e.to_string(),
            }
        })?;
        let Some(digest) = fp.sha256() else {
            return Err(ModTrustConfigError::InvalidFingerprint {
                fingerprint: trimmed.to_owned(),
                detail: "only SHA256 fingerprints are supported".to_owned(),
            });
        };
        self.sha256_fingerprints.push(digest);
        Ok(self)
    }

    /// Adds a certificate serial revocation (`KRL_SECTION_CERT_SERIAL_LIST`), globally when
    /// `ca` is `None` or scoped to certificates issued by `ca`.
    #[must_use]
    pub fn revoke_cert_serial(mut self, ca: Option<&PublicKey>, serial: u64) -> Self {
        self.scope_for_mut(ca).serials.push(serial);
        self
    }

    /// Adds an inclusive `[min, max]` certificate serial range (`KRL_SECTION_CERT_SERIAL_RANGE`).
    pub fn revoke_cert_serial_range(
        mut self,
        ca: Option<&PublicKey>,
        min: u64,
        max: u64,
    ) -> Result<Self, ModTrustConfigError> {
        if min > max {
            return Err(ModTrustConfigError::InvalidRevocationList {
                detail: format!("serial range bounds inverted: {min} > {max}"),
            });
        }
        self.scope_for_mut(ca).ranges.push((min, max));
        Ok(self)
    }

    /// Adds a certificate serial bitmap (`KRL_SECTION_CERT_SERIAL_BITMAP`) rooted at `offset`.
    pub fn revoke_cert_serial_bitmap(
        mut self,
        ca: Option<&PublicKey>,
        offset: u64,
        revoked_serials: &[u64],
    ) -> Result<Self, ModTrustConfigError> {
        encode_serial_bitmap_mpint(offset, revoked_serials).map_err(|e| {
            ModTrustConfigError::InvalidRevocationList {
                detail: e.to_string(),
            }
        })?;
        self.scope_for_mut(ca)
            .bitmaps
            .push((offset, revoked_serials.to_vec()));
        Ok(self)
    }

    /// Adds a certificate `key_id` revocation (`KRL_SECTION_CERT_KEY_ID`).
    #[must_use]
    pub fn revoke_cert_key_id(mut self, ca: Option<&PublicKey>, key_id: impl Into<String>) -> Self {
        self.scope_for_mut(ca).key_ids.push(key_id.into());
        self
    }

    /// Serializes this builder into an OpenSSH binary `SSHKRL` (`PROTOCOL.krl`) byte vector.
    pub fn to_bytes(&self) -> Result<Vec<u8>, ModSigningError> {
        let map_enc = |e: ssh_encoding::Error| ModSigningError::Encoding {
            detail: e.to_string(),
        };
        let mut out = Vec::new();
        out.extend_from_slice(SSHKRL_MAGIC);
        SSHKRL_SUPPORTED_FORMAT_VERSION
            .encode(&mut out)
            .map_err(map_enc)?;
        self.krl_version.encode(&mut out).map_err(map_enc)?;
        self.generated_date.encode(&mut out).map_err(map_enc)?;
        0u64.encode(&mut out).map_err(map_enc)?;
        ([] as [u8; 0])
            .as_slice()
            .encode(&mut out)
            .map_err(map_enc)?;
        self.comment.encode(&mut out).map_err(map_enc)?;

        if !self.global_scope.is_empty() {
            self.global_scope.encode_section(None, &mut out)?;
        }
        for (ca_kd, scope) in &self.ca_scopes {
            if !scope.is_empty() {
                scope.encode_section(Some(ca_kd), &mut out)?;
            }
        }

        if !self.explicit_keys.is_empty() {
            let mut sec = Vec::new();
            for kd in &self.explicit_keys {
                let mut kd_buf = Vec::new();
                kd.encode(&mut kd_buf).map_err(map_enc)?;
                kd_buf.encode(&mut sec).map_err(map_enc)?;
            }
            KRL_SECTION_EXPLICIT_KEY.encode(&mut out).map_err(map_enc)?;
            sec.encode(&mut out).map_err(map_enc)?;
        }

        if !self.sha256_fingerprints.is_empty() {
            let mut sec = Vec::new();
            for digest in &self.sha256_fingerprints {
                digest.as_slice().encode(&mut sec).map_err(map_enc)?;
            }
            KRL_SECTION_FINGERPRINT_SHA256
                .encode(&mut out)
                .map_err(map_enc)?;
            sec.encode(&mut out).map_err(map_enc)?;
        }

        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_key::certificate;
    use ssh_key::rand_core::OsRng;

    fn make_keypair() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).expect("generate Ed25519 key")
    }

    fn make_user_cert(
        ca: &PrivateKey,
        subject: &PrivateKey,
        key_id: &str,
        principals: &[&str],
        serial: u64,
        valid_after: u64,
        valid_before: u64,
    ) -> Certificate {
        let mut builder = certificate::Builder::new_with_random_nonce(
            &mut OsRng,
            subject.public_key(),
            valid_after,
            valid_before,
        )
        .expect("cert builder");
        builder.serial(serial).expect("serial");
        builder.key_id(key_id).expect("key_id");
        builder.cert_type(CertType::User).expect("cert_type");
        for &p in principals {
            builder.valid_principal(p).expect("principal");
        }
        builder.sign(ca).expect("sign cert")
    }

    #[test]
    fn ca_certificate_and_sshsig_verification_succeeds() {
        let ca = make_keypair();
        let signer = make_keypair();
        let cert = make_user_cert(
            &ca,
            &signer,
            "studio-ci-1",
            &["combat_mod", "publisher_a"],
            42,
            1_000,
            2_000,
        );
        let payload = b"fake-mod-binary-bytes";
        let sig_pem =
            sign_mod_bytes_with_certificate(&signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
                .expect("sign");

        let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_500))
            .with_required_principal("combat_mod");

        let receipt = policy
            .verify_bytes(payload, sig_pem.as_bytes())
            .expect("verify succeeds");
        assert_eq!(
            receipt.signer_fingerprint,
            signer.public_key().fingerprint(HashAlg::Sha256).to_string()
        );
        assert_eq!(
            receipt.ca_fingerprint.as_deref(),
            Some(
                ca.public_key()
                    .fingerprint(HashAlg::Sha256)
                    .to_string()
                    .as_str()
            )
        );
        assert_eq!(receipt.key_id.as_deref(), Some("studio-ci-1"));
        assert_eq!(receipt.principals, vec!["combat_mod", "publisher_a"]);
        assert_eq!(receipt.serial, Some(42));
        assert_eq!(receipt.namespace, DEFAULT_SIGNATURE_NAMESPACE);
        let expected_sha: [u8; 32] = Sha256::digest(payload).into();
        assert_eq!(receipt.binary_sha256, expected_sha);

        assert!(
            policy
                .with_manifest_name_principal_binding(true)
                .verify_manifest_principal(&receipt, "combat_mod")
                .is_ok()
        );
    }

    #[test]
    fn direct_pinned_public_key_and_key_rotation_work() {
        let key1 = make_keypair();
        let key2 = make_keypair();
        let payload = b"pinned-mod";
        let sig_pem =
            sign_mod_bytes_with_key(&key2, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");

        let policy =
            ModTrustPolicy::from_public_key_openssh(&key1.public_key().to_openssh().unwrap())
                .unwrap()
                .add_public_key_openssh(&key2.public_key().to_openssh().unwrap())
                .unwrap();

        let receipt = policy
            .verify_bytes(payload, sig_pem.as_bytes())
            .expect("pinned key verifies");
        assert_eq!(receipt.ca_fingerprint, None);
        assert_eq!(receipt.serial, None);
        assert!(receipt.principals.is_empty());
    }

    #[test]
    fn strict_policy_without_anchors_fails_closed() {
        let key = make_keypair();
        let payload = b"mod";
        let sig_pem =
            sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");

        let err = ModTrustPolicy::strict()
            .verify_bytes(payload, sig_pem.as_bytes())
            .expect_err("must fail closed");
        assert!(matches!(
            err,
            SignatureVerificationError::NoTrustAnchorsConfigured
        ));
    }

    #[test]
    fn tampered_binary_and_swapped_sig_fail() {
        let key = make_keypair();
        let mut payload = b"original-payload".to_vec();
        let sig_pem =
            sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, &payload).expect("sign");

        let policy =
            ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
                .unwrap();

        // Flip 1 bit
        payload[0] ^= 0x01;
        let err = policy
            .verify_bytes(&payload, sig_pem.as_bytes())
            .expect_err("tampered binary must fail");
        assert!(matches!(err, SignatureVerificationError::InvalidSignature));

        // Signature from another valid payload
        let sig_other =
            sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"different-payload")
                .expect("sign");
        let err2 = policy
            .verify_bytes(b"original-payload", sig_other.as_bytes())
            .expect_err("swapped sig must fail");
        assert!(matches!(err2, SignatureVerificationError::InvalidSignature));
    }

    #[test]
    fn malformed_pem_version_magic_and_trailing_bytes_rejected() {
        let key = make_keypair();
        let policy =
            ModTrustPolicy::from_public_key_openssh(&key.public_key().to_openssh().unwrap())
                .unwrap();

        assert!(matches!(
            policy.verify_bytes(b"a", b"not pem"),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));
        assert!(matches!(
            policy.verify_bytes(
                b"a",
                b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n"
            ),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));

        // Bad magic
        let mut bad_magic = Vec::new();
        bad_magic.extend_from_slice(b"BADMAG");
        let pem_bad_magic = pem_encode_sshsig(&bad_magic);
        assert!(matches!(
            policy.verify_bytes(b"a", pem_bad_magic.as_bytes()),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));

        // Version 2
        let mut bad_ver = Vec::new();
        bad_ver.extend_from_slice(SSHSIG_MAGIC_PREAMBLE);
        2u32.encode(&mut bad_ver).unwrap();
        let pem_bad_ver = pem_encode_sshsig(&bad_ver);
        assert!(matches!(
            policy.verify_bytes(b"a", pem_bad_ver.as_bytes()),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));

        // Trailing bytes inside envelope
        let valid_pem =
            sign_mod_bytes_with_key(&key, DEFAULT_SIGNATURE_NAMESPACE, b"a").expect("sign");
        let valid_env = RawSshSigEnvelope::decode_pem(valid_pem.as_bytes()).unwrap();
        let mut raw = Vec::new();
        valid_env.encode(&mut raw).unwrap();
        raw.push(0x99);
        let pem_trailing = pem_encode_sshsig(&raw);
        assert!(matches!(
            policy.verify_bytes(b"a", pem_trailing.as_bytes()),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));
    }

    fn pem_encode_sshsig(raw: &[u8]) -> String {
        use ssh_encoding::pem;
        let len = pem::encapsulated_len_wrapped("SSH SIGNATURE", 70, LineEnding::LF, raw.len())
            .expect("len");
        let mut buf = vec![0u8; len];
        let mut enc =
            pem::Encoder::new_wrapped("SSH SIGNATURE", 70, LineEnding::LF, &mut buf).unwrap();
        enc.encode(raw).unwrap();
        let written = enc.finish().unwrap();
        String::from_utf8(buf[..written].to_vec()).unwrap()
    }

    #[test]
    fn namespace_mismatch_and_untrusted_keys_rejected() {
        let ca = make_keypair();
        let rogue_ca = make_keypair();
        let signer = make_keypair();
        let payload = b"mod";

        let wrong_ns_sig = sign_mod_bytes_with_key(&signer, "file", payload).expect("sign");
        let key_policy =
            ModTrustPolicy::from_public_key_openssh(&signer.public_key().to_openssh().unwrap())
                .unwrap();
        assert!(matches!(
            key_policy.verify_bytes(payload, wrong_ns_sig.as_bytes()),
            Err(SignatureVerificationError::NamespaceMismatch { .. })
        ));

        // Uncertified public key against CA-only policy
        let ca_policy =
            ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap()).unwrap();
        let raw_sig =
            sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, payload).expect("sign");
        assert!(matches!(
            ca_policy.verify_bytes(payload, raw_sig.as_bytes()),
            Err(SignatureVerificationError::UntrustedPublicKey { .. })
        ));

        // Rogue CA certificate
        let rogue_cert = make_user_cert(&rogue_ca, &signer, "rogue", &["mod"], 1, 0, 10_000);
        let rogue_sig = sign_mod_bytes_with_certificate(
            &signer,
            &rogue_cert,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .expect("sign");
        let ca_policy_fixed =
            ca_policy.with_validity_policy(CertificateValidityPolicy::FixedTimestamp(5_000));
        assert!(matches!(
            ca_policy_fixed.verify_bytes(payload, rogue_sig.as_bytes()),
            Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
        ));

        // Forged certificate claiming trusted CA key_data but signed by rogue_ca
        let mut forged_raw = rogue_cert.to_bytes().unwrap();
        // Flip a byte in the nonce / key_id to break CA signature even if CA key matched:
        let ca_only_same_key_cert = make_user_cert(&ca, &signer, "ok", &["mod"], 1, 0, 10_000);
        let mut tampered_cert_bytes = ca_only_same_key_cert.to_bytes().unwrap();
        // Tamper the nonce byte (after cert_alg string + 4-byte nonce length):
        let alg_len = 4 + ED25519_CERT_ALGORITHM_ID.len();
        tampered_cert_bytes[alg_len + 4] ^= 0xff;
        let _ = &mut forged_raw;
        let forged_env = RawSshSigEnvelope {
            version: 1,
            signer: SshSigSigner::Certificate(tampered_cert_bytes),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            reserved: Vec::new(),
            hash_alg: HashAlg::Sha512,
            signature: RawSshSigEnvelope::decode_pem(raw_sig.as_bytes())
                .unwrap()
                .signature,
        };
        let forged_pem = forged_env.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            ca_policy_fixed.verify_bytes(payload, forged_pem.as_bytes()),
            Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
        ));
    }

    #[test]
    fn certificate_type_critical_options_and_validity_window_enforced() {
        let ca = make_keypair();
        let signer = make_keypair();
        let payload = b"mod";

        // Host cert rejected
        let mut host_builder =
            certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 200)
                .unwrap();
        host_builder.cert_type(CertType::Host).unwrap();
        host_builder.valid_principal("mod").unwrap();
        let host_cert = host_builder.sign(&ca).unwrap();
        let host_sig = sign_mod_bytes_with_certificate(
            &signer,
            &host_cert,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();
        let base_policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(150));
        assert!(matches!(
            base_policy.verify_bytes(payload, host_sig.as_bytes()),
            Err(SignatureVerificationError::InvalidCertificateType { .. })
        ));

        // Critical option rejected
        let mut crit_builder =
            certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 100, 200)
                .unwrap();
        crit_builder.cert_type(CertType::User).unwrap();
        crit_builder.valid_principal("mod").unwrap();
        crit_builder
            .critical_option("force-command", "/bin/sh")
            .unwrap();
        let crit_cert = crit_builder.sign(&ca).unwrap();
        let crit_sig = sign_mod_bytes_with_certificate(
            &signer,
            &crit_cert,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();
        assert!(matches!(
            base_policy.verify_bytes(payload, crit_sig.as_bytes()),
            Err(SignatureVerificationError::UnrecognizedCriticalOption { .. })
        ));

        // Validity windows: valid_after <= t < valid_before
        let timed_cert = make_user_cert(&ca, &signer, "timed", &["mod"], 7, 1_000, 2_000);
        let timed_sig = sign_mod_bytes_with_certificate(
            &signer,
            &timed_cert,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();

        let p_before = base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(999));
        assert!(matches!(
            p_before.verify_bytes(payload, timed_sig.as_bytes()),
            Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
        ));

        let p_at_start = base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_000));
        assert!(
            p_at_start
                .verify_bytes(payload, timed_sig.as_bytes())
                .is_ok()
        );

        let p_at_end = base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(2_000));
        assert!(matches!(
            p_at_end.verify_bytes(payload, timed_sig.as_bytes()),
            Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
        ));

        let p_ignore = base_policy
            .clone()
            .with_validity_policy(CertificateValidityPolicy::IgnoreTimestamps);
        assert!(p_ignore.verify_bytes(payload, timed_sig.as_bytes()).is_ok());

        // Expired under SystemClock (1970 window)
        let p_sys = base_policy.with_validity_policy(CertificateValidityPolicy::SystemClock);
        assert!(matches!(
            p_sys.verify_bytes(payload, timed_sig.as_bytes()),
            Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
        ));
    }

    #[test]
    fn revocations_principals_and_binary_size_limit_enforced() {
        let ca = make_keypair();
        let signer = make_keypair();
        let cert = make_user_cert(&ca, &signer, "k1", &["alpha_mod"], 99, 100, 500);
        let payload = b"0123456789";
        let sig =
            sign_mod_bytes_with_certificate(&signer, &cert, DEFAULT_SIGNATURE_NAMESPACE, payload)
                .unwrap();

        let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(200));

        // Revoked leaf key
        let rev_leaf = policy
            .clone()
            .revoke_key_openssh(&signer.public_key().to_openssh().unwrap())
            .unwrap();
        assert!(matches!(
            rev_leaf.verify_bytes(payload, sig.as_bytes()),
            Err(SignatureVerificationError::SignerRevoked { .. })
        ));

        // Revoked CA key via fingerprint
        let rev_ca = policy
            .clone()
            .revoke_fingerprint(&ca.public_key().fingerprint(HashAlg::Sha256).to_string())
            .unwrap();
        assert!(matches!(
            rev_ca.verify_bytes(payload, sig.as_bytes()),
            Err(SignatureVerificationError::SignerRevoked { .. })
        ));

        // Revoked serial
        let rev_serial = policy.clone().revoke_serial(99);
        assert!(matches!(
            rev_serial.verify_bytes(payload, sig.as_bytes()),
            Err(SignatureVerificationError::CertificateSerialRevoked { serial: 99 })
        ));

        // Principal mismatch
        let wrong_principal = policy.clone().with_required_principal("beta_mod");
        assert!(matches!(
            wrong_principal.verify_bytes(payload, sig.as_bytes()),
            Err(SignatureVerificationError::PrincipalMismatch { .. })
        ));

        // Manifest principal binding
        let receipt = policy.verify_bytes(payload, sig.as_bytes()).unwrap();
        let bind_policy = policy.clone().with_manifest_name_principal_binding(true);
        assert!(matches!(
            bind_policy.verify_manifest_principal(&receipt, "beta_mod"),
            Err(SignatureVerificationError::PrincipalMismatch { .. })
        ));

        // Binary too large
        let small_limit = policy.with_max_binary_bytes(4);
        assert!(matches!(
            small_limit.verify_bytes(payload, sig.as_bytes()),
            Err(SignatureVerificationError::BinaryTooLarge { size: 10, limit: 4 })
        ));
    }

    #[test]
    fn default_signature_path_preserves_multi_dot_and_extension() {
        assert_eq!(
            default_signature_path_for(Path::new("/mods/foo.dll")),
            PathBuf::from("/mods/foo.dll.sig")
        );
        assert_eq!(
            default_signature_path_for(Path::new("/mods/libmy.mod.dylib")),
            PathBuf::from("/mods/libmy.mod.dylib.sig")
        );
    }

    const EIGHT_TORSION_POINTS: [[u8; 32]; 8] = [
        [
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
        [
            0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef,
            0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88,
            0x6d, 0x53, 0xfc, 0x85,
        ],
        [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x80,
        ],
        [
            0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10,
            0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77,
            0x92, 0xac, 0x03, 0xfa,
        ],
        [
            0xec, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0x7f,
        ],
        [
            0xc7, 0x17, 0x6a, 0x70, 0x3d, 0x4d, 0xd8, 0x4f, 0xba, 0x3c, 0x0b, 0x76, 0x0d, 0x10,
            0x67, 0x0f, 0x2a, 0x20, 0x53, 0xfa, 0x2c, 0x39, 0xcc, 0xc6, 0x4e, 0xc7, 0xfd, 0x77,
            0x92, 0xac, 0x03, 0x7a,
        ],
        [
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ],
        [
            0x26, 0xe8, 0x95, 0x8f, 0xc2, 0xb2, 0x27, 0xb0, 0x45, 0xc3, 0xf4, 0x89, 0xf2, 0xef,
            0x98, 0xf0, 0xd5, 0xdf, 0xac, 0x05, 0xd3, 0xc6, 0x33, 0x39, 0xb1, 0x38, 0x02, 0x88,
            0x6d, 0x53, 0xfc, 0x05,
        ],
    ];

    // Non-canonical y = 3 + p (where p = 2^255 - 19): decompresses to valid curve point (x, 3),
    // so only `vk.as_bytes() == raw` detects non-canonicality!
    const NON_CANONICAL_Y_PLUS_P: [u8; 32] = [
        0xf0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ];

    // Non-canonical y = p = 2^255 - 19 (reduces to y = 0 order-4 torsion point).
    const NON_CANONICAL_Y_EQUALS_P: [u8; 32] = [
        0xed, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        0xff, 0x7f,
    ];

    fn raw_key_to_openssh(raw: [u8; 32]) -> String {
        PublicKey::from(Ed25519PublicKey(raw))
            .to_openssh()
            .expect("encode raw Ed25519 key")
    }

    fn add_ed25519_order_to_s(sig64: &mut [u8; 64]) {
        const L_LE: [u8; 32] = [
            0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9,
            0xde, 0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x10,
        ];
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = u16::from(sig64[32 + i]) + u16::from(L_LE[i]) + carry;
            sig64[32 + i] = (sum & 0xff) as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0);
    }

    fn build_raw_ed25519_cert_blob(
        nonce: &[u8],
        leaf_raw: [u8; 32],
        serial: u64,
        key_id: &str,
        principals: &[&str],
        valid_after: u64,
        valid_before: u64,
        ca_signing_key: &PrivateKey,
        override_ca_raw: Option<[u8; 32]>,
        tamper_ca_sig_s: bool,
    ) -> Vec<u8> {
        let mut tbs = Vec::new();
        ED25519_CERT_ALGORITHM_ID.encode(&mut tbs).unwrap();
        nonce.encode(&mut tbs).unwrap();
        Ed25519PublicKey(leaf_raw).encode(&mut tbs).unwrap();
        serial.encode(&mut tbs).unwrap();
        1u32.encode(&mut tbs).unwrap(); // CertType::User
        key_id.encode(&mut tbs).unwrap();
        let mut princ_buf = Vec::new();
        for p in principals {
            p.encode(&mut princ_buf).unwrap();
        }
        princ_buf.encode(&mut tbs).unwrap();
        valid_after.encode(&mut tbs).unwrap();
        valid_before.encode(&mut tbs).unwrap();
        OptionsMap::default().encode(&mut tbs).unwrap();
        OptionsMap::default().encode(&mut tbs).unwrap();
        ([] as [u8; 0]).as_slice().encode(&mut tbs).unwrap();

        let ca_kd = match override_ca_raw {
            Some(raw) => KeyData::Ed25519(Ed25519PublicKey(raw)),
            None => ca_signing_key.public_key().key_data().clone(),
        };
        let mut ca_buf = Vec::new();
        ca_kd.encode(&mut ca_buf).unwrap();
        ca_buf.encode(&mut tbs).unwrap();

        let ca_sig = ca_signing_key.try_sign(&tbs).unwrap();
        let final_ca_sig = if tamper_ca_sig_s {
            let mut bytes: [u8; 64] = ca_sig.as_bytes().try_into().unwrap();
            add_ed25519_order_to_s(&mut bytes);
            Signature::new(Algorithm::Ed25519, bytes.to_vec()).unwrap()
        } else {
            ca_sig
        };

        let mut cert_blob = tbs;
        final_ca_sig.encode_prefixed(&mut cert_blob).unwrap();
        cert_blob
    }

    #[test]
    fn strict_ed25519_rejects_torsion_points_non_canonical_y_and_scalar_malleability() {
        // 1. All 8 small-order Curve25519 torsion points + non-canonical y >= p rejected at config time.
        for bad_raw in EIGHT_TORSION_POINTS
            .into_iter()
            .chain([NON_CANONICAL_Y_PLUS_P, NON_CANONICAL_Y_EQUALS_P])
        {
            let openssh = raw_key_to_openssh(bad_raw);
            assert!(
                matches!(
                    ModTrustPolicy::from_ca_openssh(&openssh),
                    Err(ModTrustConfigError::WeakOrInvalidPublicKey { .. })
                ),
                "from_ca_openssh must reject weak/non-canonical point"
            );
            assert!(
                matches!(
                    ModTrustPolicy::from_public_key_openssh(&openssh),
                    Err(ModTrustConfigError::WeakOrInvalidPublicKey { .. })
                ),
                "from_public_key_openssh must reject weak/non-canonical point"
            );
        }

        // 2. Weak/non-canonical key injected inside SSHSIG envelope (Raw PublicKey & Certificate).
        let ca = make_keypair();
        let signer = make_keypair();
        let payload = b"native-mod-elf-or-macho";
        let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
            .unwrap()
            .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(500));

        let dummy_sig = Signature::new(Algorithm::Ed25519, vec![0u8; 64]).unwrap();

        for bad_raw in EIGHT_TORSION_POINTS
            .into_iter()
            .chain([NON_CANONICAL_Y_PLUS_P, NON_CANONICAL_Y_EQUALS_P])
        {
            // Raw public key signer in SSHSIG
            let env_raw = RawSshSigEnvelope {
                version: SSHSIG_SUPPORTED_VERSION,
                signer: SshSigSigner::PublicKey(KeyData::Ed25519(Ed25519PublicKey(bad_raw))),
                namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
                reserved: Vec::new(),
                hash_alg: HashAlg::Sha512,
                signature: dummy_sig.clone(),
            };
            let pem = env_raw.encode_pem_string(LineEnding::LF).unwrap();
            assert!(matches!(
                policy.verify_bytes(payload, pem.as_bytes()),
                Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
            ));

            // Leaf key inside CA-signed cert
            let cert_weak_leaf = build_raw_ed25519_cert_blob(
                b"0123456789abcdef",
                bad_raw,
                77,
                "weak-leaf",
                &["mod_a"],
                100,
                1000,
                &ca,
                None,
                false,
            );
            let env_cert_leaf = RawSshSigEnvelope {
                version: SSHSIG_SUPPORTED_VERSION,
                signer: SshSigSigner::Certificate(cert_weak_leaf),
                namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
                reserved: Vec::new(),
                hash_alg: HashAlg::Sha512,
                signature: dummy_sig.clone(),
            };
            let pem_leaf = env_cert_leaf.encode_pem_string(LineEnding::LF).unwrap();
            assert!(matches!(
                policy.verify_bytes(payload, pem_leaf.as_bytes()),
                Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
            ));

            // CA key inside cert
            let valid_leaf: [u8; 32] = match signer.public_key().key_data() {
                KeyData::Ed25519(k) => k.0,
                _ => unreachable!(),
            };
            let cert_weak_ca = build_raw_ed25519_cert_blob(
                b"0123456789abcdef",
                valid_leaf,
                78,
                "weak-ca",
                &["mod_a"],
                100,
                1000,
                &ca,
                Some(bad_raw),
                false,
            );
            let env_cert_ca = RawSshSigEnvelope {
                version: SSHSIG_SUPPORTED_VERSION,
                signer: SshSigSigner::Certificate(cert_weak_ca),
                namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
                reserved: Vec::new(),
                hash_alg: HashAlg::Sha512,
                signature: dummy_sig.clone(),
            };
            let pem_ca = env_cert_ca.encode_pem_string(LineEnding::LF).unwrap();
            assert!(matches!(
                policy.verify_bytes(payload, pem_ca.as_bytes()),
                Err(SignatureVerificationError::WeakOrInvalidPublicKey { .. })
            ));
        }

        // 3. Signature scalar malleability (S + l) rejected on BOTH payload signature and CA cert signature.
        let valid_pem =
            sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, payload).unwrap();
        let mut env = RawSshSigEnvelope::decode_pem_checked(valid_pem.as_bytes()).unwrap();
        let mut sig64: [u8; 64] = env.signature.as_bytes().try_into().unwrap();
        add_ed25519_order_to_s(&mut sig64);
        env.signature = Signature::new(Algorithm::Ed25519, sig64.to_vec()).unwrap();
        let tampered_pem = env.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            policy.verify_bytes(payload, tampered_pem.as_bytes()),
            Err(SignatureVerificationError::InvalidSignature)
        ));

        let valid_leaf: [u8; 32] = match signer.public_key().key_data() {
            KeyData::Ed25519(k) => k.0,
            _ => unreachable!(),
        };
        let tampered_cert_blob = build_raw_ed25519_cert_blob(
            b"0123456789abcdef",
            valid_leaf,
            88,
            "tampered-ca-sig",
            &["mod_a"],
            100,
            1000,
            &ca,
            None,
            true,
        );
        let mut env_c = RawSshSigEnvelope::decode_pem_checked(valid_pem.as_bytes()).unwrap();
        env_c.signer = SshSigSigner::Certificate(tampered_cert_blob);
        let tampered_cert_pem = env_c.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            policy.verify_bytes(payload, tampered_cert_pem.as_bytes()),
            Err(SignatureVerificationError::UntrustedCertificateAuthority { .. })
        ));

        // 4. Small-order R with zero scalar S rejected strictly.
        let mut bad_r_env = RawSshSigEnvelope::decode_pem_checked(valid_pem.as_bytes()).unwrap();
        let mut bad_r_sig = [0u8; 64];
        bad_r_sig[..32].copy_from_slice(&EIGHT_TORSION_POINTS[1]);
        bad_r_env.signature = Signature::new(Algorithm::Ed25519, bad_r_sig.to_vec()).unwrap();
        let bad_r_pem = bad_r_env.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            policy.verify_bytes(payload, bad_r_pem.as_bytes()),
            Err(SignatureVerificationError::InvalidSignature)
        ));
    }

    #[test]
    fn empty_binary_nonce_forever_cert_crlf_reserved_and_dedup() {
        let ca = make_keypair();
        let signer = make_keypair();
        let ca_pub = ca.public_key().to_openssh().unwrap();

        // Anchor deduplication
        let policy = ModTrustPolicy::from_ca_openssh(&format!("{ca_pub} comment-a"))
            .unwrap()
            .add_ca_openssh(&format!("{ca_pub} comment-b"))
            .unwrap()
            .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
            .unwrap()
            .add_public_key_openssh(&signer.public_key().to_openssh().unwrap())
            .unwrap();
        assert_eq!(policy.trusted_ca_keys.len(), 1);
        assert_eq!(policy.trusted_public_keys.len(), 1);

        // Empty binary rejected
        let valid_pem =
            sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, b"x").unwrap();
        assert!(matches!(
            policy.verify_bytes(b"", valid_pem.as_bytes()),
            Err(SignatureVerificationError::EmptyBinary)
        ));

        // Boundary at exact max_binary_bytes passes, +1 fails
        let exact_policy = policy.clone().with_max_binary_bytes(4);
        let p4 = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, b"1234").unwrap();
        assert!(exact_policy.verify_bytes(b"1234", p4.as_bytes()).is_ok());
        assert!(matches!(
            exact_policy.verify_bytes(b"12345", p4.as_bytes()),
            Err(SignatureVerificationError::BinaryTooLarge { size: 5, limit: 4 })
        ));

        // CRLF PEM line endings accepted
        let crlf_pem = p4.replace('\n', "\r\n");
        assert!(
            exact_policy
                .verify_bytes(b"1234", crlf_pem.as_bytes())
                .is_ok()
        );

        // Non-empty `reserved` in SSHSIG: passes if signed over exact reserved, fails if mutated
        let reserved_bytes = b"ext=reserved-meta";
        let digest = HashAlg::Sha512.digest(b"1234");
        let sd = encode_signed_data(
            DEFAULT_SIGNATURE_NAMESPACE,
            reserved_bytes,
            HashAlg::Sha512,
            &digest,
        )
        .unwrap();
        let res_env = RawSshSigEnvelope {
            version: SSHSIG_SUPPORTED_VERSION,
            signer: SshSigSigner::PublicKey(signer.public_key().key_data().clone()),
            namespace: DEFAULT_SIGNATURE_NAMESPACE.to_owned(),
            reserved: reserved_bytes.to_vec(),
            hash_alg: HashAlg::Sha512,
            signature: signer.try_sign(&sd).unwrap(),
        };
        let res_pem = res_env.encode_pem_string(LineEnding::LF).unwrap();
        assert!(policy.verify_bytes(b"1234", res_pem.as_bytes()).is_ok());

        let mut tampered_res_env = res_env;
        tampered_res_env.reserved = b"tampered".to_vec();
        let tampered_res_pem = tampered_res_env.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            policy.verify_bytes(b"1234", tampered_res_pem.as_bytes()),
            Err(SignatureVerificationError::InvalidSignature)
        ));

        // Empty certificate nonce rejected
        let signer_raw: [u8; 32] = match signer.public_key().key_data() {
            KeyData::Ed25519(k) => k.0,
            _ => unreachable!(),
        };
        let empty_nonce_cert = build_raw_ed25519_cert_blob(
            b"",
            signer_raw,
            10,
            "empty-nonce",
            &["mod"],
            0,
            u64::MAX,
            &ca,
            None,
            false,
        );
        let mut env_nonce = RawSshSigEnvelope::decode_pem_checked(p4.as_bytes()).unwrap();
        env_nonce.signer = SshSigSigner::Certificate(empty_nonce_cert);
        let env_nonce_pem = env_nonce.encode_pem_string(LineEnding::LF).unwrap();
        assert!(matches!(
            policy.verify_bytes(b"1234", env_nonce_pem.as_bytes()),
            Err(SignatureVerificationError::MalformedSignature { .. })
        ));

        // "Forever" certificate (valid_after = 0, valid_before = u64::MAX)
        let forever_cert = build_raw_ed25519_cert_blob(
            b"forever-nonce-01",
            signer_raw,
            42,
            "forever",
            &["mod"],
            0,
            u64::MAX,
            &ca,
            None,
            false,
        );
        let mut forever_b64 = vec![0u8; forever_cert.len().div_ceil(3) * 4];
        let encoded_forever = {
            use ssh_encoding::Base64Writer;
            let mut w = Base64Writer::new(&mut forever_b64).unwrap();
            w.write(&forever_cert).unwrap();
            w.finish().unwrap().to_owned()
        };
        let forever_line = format!("{ED25519_CERT_ALGORITHM_ID} {encoded_forever} forever");
        let forever_sig = sign_mod_bytes_with_openssh_certificate(
            &signer,
            &forever_line,
            DEFAULT_SIGNATURE_NAMESPACE,
            b"1234",
        )
        .unwrap();
        assert!(policy.verify_bytes(b"1234", forever_sig.as_bytes()).is_ok());
        assert!(
            policy
                .clone()
                .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(u64::MAX - 1))
                .verify_bytes(b"1234", forever_sig.as_bytes())
                .is_ok()
        );
        assert!(matches!(
            policy
                .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(u64::MAX))
                .verify_bytes(b"1234", forever_sig.as_bytes()),
            Err(SignatureVerificationError::CertificateExpiredOrNotYetValid { .. })
        ));
    }

    #[test]
    fn krl_sections_scoping_signed_anti_rollback_and_spec_parser() {
        let ca1 = make_keypair();
        let ca2 = make_keypair();
        let signer_a = make_keypair();
        let signer_b = make_keypair();
        let krl_delegate = make_keypair();

        let payload = b"compiled-mod-bytes";
        let cert_ca1_s7 = make_user_cert(&ca1, &signer_a, "kid-alpha", &["m"], 7, 0, 4_000_000_000);
        let cert_ca2_s7 = make_user_cert(&ca2, &signer_b, "kid-beta", &["m"], 7, 0, 4_000_000_000);
        let cert_ca1_s80 = make_user_cert(&ca1, &signer_a, "kid-hi", &["m"], 107, 0, 4_000_000_000);
        let cert_ca1_s200 =
            make_user_cert(&ca1, &signer_a, "kid-ok", &["m"], 200, 0, 4_000_000_000);
        let cert_krl = make_user_cert(
            &ca1,
            &krl_delegate,
            "krl-signer",
            &["krl"],
            999,
            0,
            4_000_000_000,
        );

        let sig_ca1_s7 = sign_mod_bytes_with_certificate(
            &signer_a,
            &cert_ca1_s7,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();
        let sig_ca2_s7 = sign_mod_bytes_with_certificate(
            &signer_b,
            &cert_ca2_s7,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();
        let sig_ca1_s80 = sign_mod_bytes_with_certificate(
            &signer_a,
            &cert_ca1_s80,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();
        let sig_ca1_s200 = sign_mod_bytes_with_certificate(
            &signer_a,
            &cert_ca1_s200,
            DEFAULT_SIGNATURE_NAMESPACE,
            payload,
        )
        .unwrap();

        let base = ModTrustPolicy::from_ca_openssh(&ca1.public_key().to_openssh().unwrap())
            .unwrap()
            .add_ca_openssh(&ca2.public_key().to_openssh().unwrap())
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(100));

        // 1. CA-scoped serial + high-bit bitmap (offset 100, bit 7 => 0x80 mpint with 0x00 prefix)
        // + global key_id + explicit key + sha256 fingerprint
        let krl_v5 = KrlBuilder::new()
            .version(5)
            .comment("scoped-and-global")
            .revoke_cert_serial(Some(ca1.public_key()), 7)
            .revoke_cert_serial_bitmap(Some(ca1.public_key()), 100, &[107])
            .unwrap()
            .revoke_cert_serial_range(None, 500, 510)
            .unwrap()
            .revoke_cert_key_id(Some(ca2.public_key()), "kid-other")
            .to_bytes()
            .unwrap();

        let krl_sig_by_delegate = sign_mod_bytes_with_certificate(
            &krl_delegate,
            &cert_krl,
            DEFAULT_KRL_SIGNATURE_NAMESPACE,
            &krl_v5,
        )
        .unwrap();
        let krl_sig_wrong_ns =
            sign_mod_bytes_with_key(&ca1, DEFAULT_SIGNATURE_NAMESPACE, &krl_v5).unwrap();

        // Wrong namespace rejected (mod signature cannot replay as signed KRL!)
        assert!(matches!(
            base.clone()
                .add_signed_krl_bytes(&krl_v5, krl_sig_wrong_ns.as_bytes()),
            Err(ModTrustConfigError::KrlSignatureInvalid { .. })
        ));
        // Untrusted signer rejected
        let rogue = make_keypair();
        let rogue_krl_sig =
            sign_mod_bytes_with_key(&rogue, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v5).unwrap();
        assert!(matches!(
            base.clone()
                .add_signed_krl_bytes(&krl_v5, rogue_krl_sig.as_bytes()),
            Err(ModTrustConfigError::KrlSignatureInvalid { .. })
        ));
        // Tampered KRL rejected
        let mut tampered_krl = krl_v5.clone();
        *tampered_krl.last_mut().unwrap() ^= 0xff;
        assert!(matches!(
            base.clone()
                .add_signed_krl_bytes(&tampered_krl, krl_sig_by_delegate.as_bytes()),
            Err(ModTrustConfigError::KrlSignatureInvalid { .. })
        ));

        // Valid signed KRL by delegated cert passes
        let p_v5 = base
            .clone()
            .add_signed_krl_bytes(&krl_v5, krl_sig_by_delegate.as_bytes())
            .unwrap();
        assert_eq!(p_v5.applied_krl_version(), Some(5));

        // CA1 serial 7 is revoked, CA2 serial 7 is still valid!
        assert!(matches!(
            p_v5.verify_bytes(payload, sig_ca1_s7.as_bytes()),
            Err(SignatureVerificationError::CertificateSerialRevoked { serial: 7 })
        ));
        assert!(p_v5.verify_bytes(payload, sig_ca2_s7.as_bytes()).is_ok());
        // CA1 bitmap serial 107 is revoked, serial 200 is valid
        assert!(matches!(
            p_v5.verify_bytes(payload, sig_ca1_s80.as_bytes()),
            Err(SignatureVerificationError::CertificateSerialRevoked { serial: 107 })
        ));
        let receipt_200 = p_v5.verify_bytes(payload, sig_ca1_s200.as_bytes()).unwrap();

        // Anti-rollback: KRL version 4 < 5 rejected even if properly signed by Root CA!
        let krl_v4 = KrlBuilder::new().version(4).to_bytes().unwrap();
        let sig_v4 =
            sign_mod_bytes_with_key(&ca1, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v4).unwrap();
        assert!(matches!(
            p_v5.clone()
                .add_signed_krl_bytes(&krl_v4, sig_v4.as_bytes()),
            Err(ModTrustConfigError::KrlVersionRollback {
                offered: 4,
                min_required: 5,
            })
        ));
        assert!(matches!(
            base.clone().with_min_krl_version(6).add_krl_bytes(&krl_v5),
            Err(ModTrustConfigError::KrlVersionRollback {
                offered: 5,
                min_required: 6,
            })
        ));

        // 2. key_id revocation, range edge [u64::MAX - 5, u64::MAX], explicit key, sha256 fp
        let krl_v6 = KrlBuilder::new()
            .version(6)
            .revoke_cert_key_id(None, "kid-ok")
            .revoke_cert_serial_range(None, u64::MAX - 5, u64::MAX)
            .unwrap()
            .revoke_key_openssh(&signer_b.public_key().to_openssh().unwrap())
            .unwrap()
            .to_bytes()
            .unwrap();
        let sig_v6 =
            sign_mod_bytes_with_key(&ca1, DEFAULT_KRL_SIGNATURE_NAMESPACE, &krl_v6).unwrap();
        let p_v6 = p_v5
            .add_signed_krl_bytes(&krl_v6, sig_v6.as_bytes())
            .unwrap();
        assert!(matches!(
            p_v6.verify_bytes(payload, sig_ca1_s200.as_bytes()),
            Err(SignatureVerificationError::CertificateKeyIdRevoked { .. })
        ));
        assert!(matches!(
            p_v6.verify_receipt_still_trusted(&receipt_200),
            Err(SignatureVerificationError::CertificateKeyIdRevoked { .. })
        ));
        assert!(matches!(
            p_v6.verify_bytes(payload, sig_ca2_s7.as_bytes()),
            Err(SignatureVerificationError::SignerRevoked { .. })
        ));

        // 3. KRL fail-closed sections (SHA1 = 0x03, SIGNATURE = 0x04, critical extension, non-critical extension)
        let make_single_section_krl = |sec_type: u8, payload: &[u8]| {
            let mut b = Vec::new();
            b.extend_from_slice(SSHKRL_MAGIC);
            1u32.encode(&mut b).unwrap();
            9u64.encode(&mut b).unwrap();
            0u64.encode(&mut b).unwrap();
            0u64.encode(&mut b).unwrap();
            ([] as [u8; 0]).as_slice().encode(&mut b).unwrap();
            "".encode(&mut b).unwrap();
            sec_type.encode(&mut b).unwrap();
            payload.encode(&mut b).unwrap();
            b
        };
        assert!(matches!(
            base.clone().add_krl_bytes(&make_single_section_krl(
                KRL_SECTION_FINGERPRINT_SHA1,
                &[1, 2]
            )),
            Err(ModTrustConfigError::InvalidRevocationList { .. })
        ));
        assert!(matches!(
            base.clone()
                .add_krl_bytes(&make_single_section_krl(KRL_SECTION_SIGNATURE, &[1, 2])),
            Err(ModTrustConfigError::InvalidRevocationList { .. })
        ));

        let mut ext_crit = Vec::new();
        "vendor.crit@example".encode(&mut ext_crit).unwrap();
        1u8.encode(&mut ext_crit).unwrap();
        [42u8].as_slice().encode(&mut ext_crit).unwrap();
        assert!(matches!(
            base.clone()
                .add_krl_bytes(&make_single_section_krl(KRL_SECTION_EXTENSION, &ext_crit)),
            Err(ModTrustConfigError::InvalidRevocationList { .. })
        ));

        let mut ext_opt = Vec::new();
        "vendor.opt@example".encode(&mut ext_opt).unwrap();
        0u8.encode(&mut ext_opt).unwrap();
        [42u8].as_slice().encode(&mut ext_opt).unwrap();
        assert!(
            base.clone()
                .add_krl_bytes(&make_single_section_krl(KRL_SECTION_EXTENSION, &ext_opt))
                .is_ok()
        );

        // 4. Text revocation spec parser (transactional on invalid line)
        let extra = make_keypair();
        let spec = format!(
            "# OpenSSH KRL text spec\n\nserial: 300-310\nserial: 350\nid: \"rev-id\"\nhash: {}\n",
            extra.public_key().fingerprint(HashAlg::Sha256)
        );
        let p_spec = base.clone().add_revocation_spec_str(&spec).unwrap();
        assert!(p_spec.is_cert_serial_revoked(&ca1.public_key().fingerprint(HashAlg::Sha256), 305));
        assert!(
            p_spec.is_cert_key_id_revoked(&ca1.public_key().fingerprint(HashAlg::Sha256), "rev-id")
        );
        assert!(matches!(
            base.add_revocation_spec_str("serial: 10-5"),
            Err(ModTrustConfigError::InvalidRevocationList { .. })
        ));
    }

    mod prop_tests {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn arbitrary_payload_bitflip_never_verifies(
                payload in proptest::collection::vec(any::<u8>(), 1..256),
                flip_byte in any::<prop::sample::Index>(),
                flip_bit in 0u8..8,
            ) {
                let signer = make_keypair();
                let policy = ModTrustPolicy::from_public_key_openssh(
                    &signer.public_key().to_openssh().unwrap()
                ).unwrap();
                let pem = sign_mod_bytes_with_key(&signer, DEFAULT_SIGNATURE_NAMESPACE, &payload).unwrap();
                prop_assert!(policy.verify_bytes(&payload, pem.as_bytes()).is_ok());

                let mut tampered_payload = payload.clone();
                let p_idx = flip_byte.index(tampered_payload.len());
                tampered_payload[p_idx] ^= 1u8 << flip_bit;
                prop_assert!(policy.verify_bytes(&tampered_payload, pem.as_bytes()).is_err());

                let mut env = RawSshSigEnvelope::decode_pem_checked(pem.as_bytes()).unwrap();
                let mut sig64: [u8; 64] = env.signature.as_bytes().try_into().unwrap();
                let s_idx = flip_byte.index(64);
                sig64[s_idx] ^= 1u8 << flip_bit;
                env.signature = Signature::new(Algorithm::Ed25519, sig64.to_vec()).unwrap();
                let bad_pem = env.encode_pem_string(LineEnding::LF).unwrap();
                prop_assert!(policy.verify_bytes(&payload, bad_pem.as_bytes()).is_err());
            }

            #[test]
            fn krl_serial_and_key_id_roundtrip(
                serials in proptest::collection::vec(0u64..1000, 1..16),
                bitmap_deltas in proptest::collection::vec(0u64..128, 1..16),
                offset in 1000u64..2000,
                range_start in 3000u64..3500,
                range_span in 0u64..100,
                probe in 0u64..3700,
                kid in "[a-z0-9_-]{1,16}",
            ) {
                let ca = make_keypair();
                let ca_pk = ca.public_key();
                let ca_fp = ca_pk.fingerprint(HashAlg::Sha256);

                let mut builder = KrlBuilder::new().version(1).revoke_cert_key_id(Some(ca_pk), kid.clone());
                for &s in &serials {
                    builder = builder.revoke_cert_serial(Some(ca_pk), s);
                }
                let bm_serials: Vec<u64> = bitmap_deltas.iter().map(|d| offset + d).collect();
                builder = builder
                    .revoke_cert_serial_bitmap(Some(ca_pk), offset, &bm_serials)
                    .unwrap()
                    .revoke_cert_serial_range(Some(ca_pk), range_start, range_start + range_span)
                    .unwrap();

                let krl_bytes = builder.to_bytes().unwrap();
                let policy = ModTrustPolicy::from_ca_openssh(&ca_pk.to_openssh().unwrap())
                    .unwrap()
                    .add_krl_bytes(&krl_bytes)
                    .unwrap();

                let expected = serials.contains(&probe)
                    || bm_serials.contains(&probe)
                    || (range_start <= probe && probe <= range_start + range_span);
                prop_assert_eq!(policy.is_cert_serial_revoked(&ca_fp, probe), expected);
                prop_assert!(policy.is_cert_key_id_revoked(&ca_fp, &kid));
            }

            #[test]
            fn parsers_never_panic_on_arbitrary_bytes(
                data in proptest::collection::vec(any::<u8>(), 0..512),
                split_idx in any::<prop::sample::Index>(),
            ) {
                let key = make_keypair();
                let policy = ModTrustPolicy::from_public_key_openssh(
                    &key.public_key().to_openssh().unwrap()
                ).unwrap();
                let at = if data.is_empty() { 0 } else { split_idx.index(data.len()) };
                let (a, b) = data.split_at(at);
                let _ = policy.verify_bytes(a, b);
                let _ = policy.clone().add_krl_bytes(&data);
                let _ = policy.clone().add_signed_krl_bytes(a, b);
                if let Ok(s) = std::str::from_utf8(&data) {
                    let _ = policy.add_revocation_spec_str(s);
                }
            }
        }
    }
}
