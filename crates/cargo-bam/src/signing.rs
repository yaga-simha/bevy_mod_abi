//! Pure-Rust Ed25519 keygen, OpenSSH `SSHSIG` `.sig` detached signing, and strict verification.

use crate::CliError;
use sha2::{Digest, Sha256};
use signature::Signer;
use ssh_encoding::pem::PemLabel;
use ssh_encoding::{CheckedSum, Decode, DecodePem, Encode, EncodePem, LineEnding, Reader, Writer};
use ssh_key::public::KeyData;
use ssh_key::rand_core::OsRng;
use ssh_key::{Algorithm, Fingerprint, HashAlg, PrivateKey, PublicKey, Signature};
use std::fs;
use std::path::{Path, PathBuf};

/// Default `SSHSIG` namespace used when signing `bevy_abi_mod` guest libraries.
pub const DEFAULT_NAMESPACE: &str = "bevy_abi_mod.v1";

const SSHSIG_MAGIC: &[u8] = b"SSHSIG";
const SSHSIG_VERSION: u32 = 1;

/// Result of generating an Ed25519 keypair.
#[derive(Debug, Clone)]
pub struct KeygenOutput {
    /// Path to the OpenSSH private key file.
    pub private_key: PathBuf,
    /// Path to the OpenSSH `.pub` public key file.
    pub public_key: PathBuf,
    /// SHA-256 fingerprint string.
    pub fingerprint: String,
}

/// Result of signing a shared library binary.
#[derive(Debug, Clone)]
pub struct SignOutput {
    /// Path to the written `.sig` PEM file.
    pub signature_path: PathBuf,
    /// Signer SHA-256 fingerprint.
    pub signer_fingerprint: String,
    /// Hex-encoded SHA-256 digest of the signed binary.
    pub binary_sha256: String,
}

/// Generates an Ed25519 OpenSSH keypair (`<out>` + `<out>.pub`).
pub fn generate_keypair(
    out_prefix: &Path,
    comment: Option<&str>,
) -> Result<KeygenOutput, CliError> {
    if let Some(parent) = out_prefix.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }

    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
        .map_err(|e| CliError::Crypto(format!("failed to generate Ed25519 keypair: {e}")))?;
    if let Some(c) = comment {
        key.set_comment(c);
    }

    key.write_openssh_file(out_prefix, LineEnding::LF)
        .map_err(|e| {
            CliError::Crypto(format!(
                "failed to write private key {}: {e}",
                out_prefix.display()
            ))
        })?;

    let mut pub_os = out_prefix.as_os_str().to_os_string();
    pub_os.push(".pub");
    let pub_path = PathBuf::from(pub_os);
    let mut pub_text = key
        .public_key()
        .to_openssh()
        .map_err(|e| CliError::Crypto(format!("failed to encode OpenSSH public key: {e}")))?;
    pub_text.push('\n');
    fs::write(&pub_path, pub_text.as_bytes())?;

    let fp = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    Ok(KeygenOutput {
        private_key: out_prefix.to_path_buf(),
        public_key: pub_path,
        fingerprint: fp,
    })
}

/// Default `.sig` sidecar path (`<mod_path>.sig`).
pub fn default_sig_path(mod_path: &Path) -> PathBuf {
    let mut os = mod_path.as_os_str().to_os_string();
    os.push(".sig");
    PathBuf::from(os)
}

/// Signs `mod_path` into a detached OpenSSH `SSHSIG` `.sig` file.
pub fn sign_mod_file(
    key_path: &Path,
    mod_path: &Path,
    out_sig: Option<&Path>,
    namespace: &str,
) -> Result<SignOutput, CliError> {
    if namespace.is_empty() {
        return Err(CliError::InvalidArgument(
            "`--namespace` must not be empty".to_owned(),
        ));
    }
    let binary_bytes = fs::read(mod_path)?;
    if binary_bytes.is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "mod binary {} is empty",
            mod_path.display()
        )));
    }
    let signing_key = PrivateKey::read_openssh_file(key_path).map_err(|e| {
        CliError::Crypto(format!(
            "failed to read signing key {}: {e}",
            key_path.display()
        ))
    })?;
    if signing_key.algorithm() != Algorithm::Ed25519 {
        return Err(CliError::Crypto(format!(
            "signing key {} uses unsupported algorithm {}",
            key_path.display(),
            signing_key.algorithm()
        )));
    }

    let hash_alg = HashAlg::Sha512;
    let digest = hash_alg.digest(&binary_bytes);
    let signed_data = encode_signed_data(namespace, &[], hash_alg, &digest)
        .map_err(|e| CliError::Crypto(format!("failed to encode SSHSIG signed data: {e}")))?;
    let sig = signing_key
        .try_sign(&signed_data)
        .map_err(|e| CliError::Crypto(format!("failed to sign payload: {e}")))?;

    let envelope = RawSshSigEnvelope {
        version: SSHSIG_VERSION,
        signer_pubkey: signing_key.public_key().key_data().clone(),
        namespace: namespace.to_owned(),
        reserved: Vec::new(),
        hash_alg,
        signature: sig,
    };
    let pem = envelope
        .encode_pem_string(LineEnding::LF)
        .map_err(|e| CliError::Crypto(format!("failed to encode SSHSIG PEM: {e}")))?;

    let sig_out = out_sig
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_sig_path(mod_path));
    if let Some(parent) = sig_out.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    fs::write(&sig_out, pem.as_bytes())?;

    let sha256: [u8; 32] = Sha256::digest(&binary_bytes).into();
    let sha_hex: String = sha256.iter().map(|b| format!("{b:02x}")).collect();
    Ok(SignOutput {
        signature_path: sig_out,
        signer_fingerprint: signing_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string(),
        binary_sha256: sha_hex,
    })
}

/// Verifies `mod_path` against its detached `SSHSIG` `.sig` sidecar and pinned OpenSSH Ed25519
/// public keys (`pubkey_paths`).
pub fn verify_mod_file(
    mod_path: &Path,
    sig_path: Option<&Path>,
    pubkey_paths: &[PathBuf],
    namespace: &str,
) -> Result<Fingerprint, CliError> {
    if pubkey_paths.is_empty() {
        return Err(CliError::InvalidArgument(
            "at least one `--pubkey <key.pub>` is required".to_owned(),
        ));
    }
    let mut trusted = Vec::with_capacity(pubkey_paths.len());
    for p in pubkey_paths {
        let text = fs::read_to_string(p)?;
        let pk = PublicKey::from_openssh(text.trim()).map_err(|e| {
            CliError::Crypto(format!("invalid OpenSSH pubkey {}: {e}", p.display()))
        })?;
        if pk.algorithm() != Algorithm::Ed25519 {
            return Err(CliError::Crypto(format!(
                "pubkey {} must be Ed25519",
                p.display()
            )));
        }
        trusted.push(pk);
    }

    let sig_file = sig_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_sig_path(mod_path));
    let binary_bytes = fs::read(mod_path)?;
    if binary_bytes.is_empty() {
        return Err(CliError::Crypto("mod binary is empty".to_owned()));
    }
    let sig_bytes = fs::read(&sig_file)?;

    let envelope = RawSshSigEnvelope::decode_pem(&sig_bytes)
        .map_err(|e| CliError::Crypto(format!("invalid SSHSIG PEM {}: {e}", sig_file.display())))?;
    if envelope.namespace != namespace {
        return Err(CliError::Crypto(format!(
            "namespace mismatch: expected '{namespace}', got '{}'",
            envelope.namespace
        )));
    }
    if envelope.signature.algorithm() != Algorithm::Ed25519 {
        return Err(CliError::Crypto(
            "signature algorithm must be Ed25519".to_owned(),
        ));
    }

    if !trusted
        .iter()
        .any(|pk| pk.key_data() == &envelope.signer_pubkey)
    {
        return Err(CliError::Crypto(format!(
            "untrusted signer fingerprint {}",
            envelope.signer_pubkey.fingerprint(HashAlg::Sha256)
        )));
    }

    let vk = validate_strict_ed25519_kd(&envelope.signer_pubkey)?;
    let sig_arr: [u8; 64] = envelope
        .signature
        .as_bytes()
        .try_into()
        .map_err(|_| CliError::Crypto("invalid Ed25519 signature byte length".to_owned()))?;
    let dalek_sig = ed25519_dalek::Signature::from_bytes(&sig_arr);

    let digest = envelope.hash_alg.digest(&binary_bytes);
    let signed_data = encode_signed_data(
        &envelope.namespace,
        &envelope.reserved,
        envelope.hash_alg,
        &digest,
    )
    .map_err(|e| CliError::Crypto(format!("failed to reconstruct SSHSIG signed payload: {e}")))?;

    vk.verify_strict(&signed_data, &dalek_sig)
        .map_err(|_| CliError::Crypto("cryptographic signature verification failed".to_owned()))?;

    Ok(envelope.signer_pubkey.fingerprint(HashAlg::Sha256))
}

fn is_canonical_ed25519_y(raw: &[u8; 32]) -> bool {
    let mut y_bytes = *raw;
    y_bytes[31] &= 0x7f;
    let limbs = [
        u64::from_le_bytes(y_bytes[0..8].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[8..16].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[16..24].try_into().unwrap()),
        u64::from_le_bytes(y_bytes[24..32].try_into().unwrap()),
    ];
    const P_LIMBS: [u64; 4] = [
        0xffff_ffff_ffff_ffed,
        u64::MAX,
        u64::MAX,
        0x7fff_ffff_ffff_ffff,
    ];
    (limbs[3], limbs[2], limbs[1], limbs[0]) < (P_LIMBS[3], P_LIMBS[2], P_LIMBS[1], P_LIMBS[0])
}

fn validate_strict_ed25519_kd(kd: &KeyData) -> Result<ed25519_dalek::VerifyingKey, CliError> {
    let raw = match kd {
        KeyData::Ed25519(pk) => &pk.0,
        other => {
            return Err(CliError::Crypto(format!(
                "expected Ed25519 key, found '{}'",
                other.algorithm()
            )));
        }
    };
    if !is_canonical_ed25519_y(raw) {
        return Err(CliError::Crypto(
            "non-canonical Ed25519 public key encoding".to_owned(),
        ));
    }
    let vk = ed25519_dalek::VerifyingKey::from_bytes(raw)
        .map_err(|e| CliError::Crypto(format!("invalid Ed25519 public key: {e}")))?;
    if vk.is_weak() {
        return Err(CliError::Crypto(
            "small-order Curve25519 torsion key rejected".to_owned(),
        ));
    }
    Ok(vk)
}

fn encode_signed_data(
    namespace: &str,
    reserved: &[u8],
    hash_alg: HashAlg,
    digest: &[u8],
) -> ssh_encoding::Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(SSHSIG_MAGIC);
    namespace.encode(&mut out)?;
    reserved.encode(&mut out)?;
    hash_alg.as_str().encode(&mut out)?;
    digest.encode(&mut out)?;
    Ok(out)
}

struct RawSshSigEnvelope {
    version: u32,
    signer_pubkey: KeyData,
    namespace: String,
    reserved: Vec<u8>,
    hash_alg: HashAlg,
    signature: Signature,
}

impl PemLabel for RawSshSigEnvelope {
    const PEM_LABEL: &'static str = "SSH SIGNATURE";
}

impl Decode for RawSshSigEnvelope {
    type Error = ssh_key::Error;

    fn decode(reader: &mut impl Reader) -> Result<Self, Self::Error> {
        let mut magic = [0u8; 6];
        reader.read(&mut magic)?;
        if magic != SSHSIG_MAGIC {
            return Err(ssh_key::Error::FormatEncoding);
        }
        let version = u32::decode(reader)?;
        if version != SSHSIG_VERSION {
            return Err(ssh_key::Error::Version { number: version });
        }
        let signer_bytes = Vec::<u8>::decode(reader)?;
        let mut pk_reader = signer_bytes.as_slice();
        let signer_pubkey = KeyData::decode(&mut pk_reader)?;
        if !pk_reader.is_finished() {
            return Err(ssh_key::Error::TrailingData {
                remaining: pk_reader.remaining_len(),
            });
        }
        let namespace = String::decode(reader)?;
        let reserved = Vec::<u8>::decode(reader)?;
        let hash_alg = HashAlg::decode(reader)?;
        let signature = reader.read_prefixed(|r| {
            let sig = Signature::decode(r)?;
            if !r.is_finished() {
                return Err(ssh_key::Error::from(ssh_encoding::Error::TrailingData {
                    remaining: r.remaining_len(),
                }));
            }
            Ok::<_, ssh_key::Error>(sig)
        })?;
        if !reader.is_finished() {
            return Err(ssh_key::Error::TrailingData {
                remaining: reader.remaining_len(),
            });
        }
        Ok(Self {
            version,
            signer_pubkey,
            namespace,
            reserved,
            hash_alg,
            signature,
        })
    }
}

impl Encode for RawSshSigEnvelope {
    fn encoded_len(&self) -> ssh_encoding::Result<usize> {
        let mut signer_buf = Vec::new();
        self.signer_pubkey.encode(&mut signer_buf)?;
        let mut sig_buf = Vec::new();
        self.signature.encode(&mut sig_buf)?;
        [
            SSHSIG_MAGIC.len(),
            self.version.encoded_len()?,
            signer_buf.encoded_len()?,
            self.namespace.encoded_len()?,
            self.reserved.encoded_len()?,
            self.hash_alg.as_str().encoded_len()?,
            sig_buf.encoded_len()?,
        ]
        .checked_sum()
    }

    fn encode(&self, writer: &mut impl Writer) -> ssh_encoding::Result<()> {
        writer.write(SSHSIG_MAGIC)?;
        self.version.encode(writer)?;
        let mut signer_buf = Vec::new();
        self.signer_pubkey.encode(&mut signer_buf)?;
        signer_buf.encode(writer)?;
        self.namespace.encode(writer)?;
        self.reserved.encode(writer)?;
        self.hash_alg.as_str().encode(writer)?;
        let mut sig_buf = Vec::new();
        self.signature.encode(&mut sig_buf)?;
        sig_buf.encode(writer)?;
        Ok(())
    }
}
