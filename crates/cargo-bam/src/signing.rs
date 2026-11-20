//! Pure-Rust Ed25519 keygen, OpenSSH `SSHSIG` `.sig` detached signing, and strict verification,
//! delegated to [`bevy_abi_mod_trust`].

use crate::{CliError, atomic_write_bytes, read_file_bounded};
use bevy_abi_mod_trust::{
    DEFAULT_MAX_MOD_BINARY_BYTES, DEFAULT_SIGNATURE_NAMESPACE, Fingerprint,
    MAX_SIGNATURE_SIDECAR_BYTES, ModTrustPolicy, default_signature_path_for,
    sign_mod_bytes_with_key, ssh_key::PrivateKey, write_openssh_keypair,
};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Default `SSHSIG` namespace used when signing `bevy_abi_mod` guest libraries.
pub const DEFAULT_NAMESPACE: &str = DEFAULT_SIGNATURE_NAMESPACE;

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
    /// Hex-encoded SHA-256 digest of the signed binary bytes.
    pub binary_sha256: String,
}

/// Generates an Ed25519 OpenSSH keypair (`<out>` + `<out>.pub`).
pub fn generate_keypair(
    out_prefix: &Path,
    comment: Option<&str>,
) -> Result<KeygenOutput, CliError> {
    let generated = write_openssh_keypair(out_prefix, comment)
        .map_err(|e| CliError::Crypto(format!("failed to generate Ed25519 keypair: {e}")))?;
    Ok(KeygenOutput {
        private_key: generated.private_key,
        public_key: generated.public_key,
        fingerprint: generated.fingerprint,
    })
}

/// Default `.sig` sidecar path (`<mod_path>.sig`).
pub fn default_sig_path(mod_path: &Path) -> PathBuf {
    default_signature_path_for(mod_path)
}

/// Signs `mod_path` into a detached OpenSSH `SSHSIG` `.sig` file (written atomically).
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
    let binary_bytes = read_file_bounded(mod_path, DEFAULT_MAX_MOD_BINARY_BYTES, "mod binary")?;
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
    let pem = sign_mod_bytes_with_key(&signing_key, namespace, &binary_bytes)
        .map_err(|e| CliError::Crypto(e.to_string()))?;

    let sig_out = out_sig
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_sig_path(mod_path));
    atomic_write_bytes(&sig_out, pem.as_bytes())?;

    let sha256: [u8; 32] = Sha256::digest(&binary_bytes).into();
    let sha_hex: String = sha256.iter().map(|b| format!("{b:02x}")).collect();
    Ok(SignOutput {
        signature_path: sig_out,
        signer_fingerprint: signing_key
            .public_key()
            .fingerprint(bevy_abi_mod_trust::HashAlg::Sha256)
            .to_string(),
        binary_sha256: sha_hex,
    })
}

/// Builds a strict [`ModTrustPolicy`] from CLI `--pubkey`, `--ca`, `--krl`, `--principal`, and
/// `--namespace` arguments.
pub(crate) fn build_cli_trust_policy(
    pubkey_paths: &[PathBuf],
    ca_key_paths: &[PathBuf],
    krl_paths: &[PathBuf],
    required_principal: Option<&str>,
    namespace: &str,
) -> Result<ModTrustPolicy, CliError> {
    if pubkey_paths.is_empty() && ca_key_paths.is_empty() {
        return Err(CliError::InvalidArgument(
            "at least one `--pubkey <key.pub>` or `--ca <ca.pub>` is required".to_owned(),
        ));
    }
    if namespace.is_empty() {
        return Err(CliError::InvalidArgument(
            "`--namespace` must not be empty".to_owned(),
        ));
    }

    let mut policy = ModTrustPolicy::strict()
        .with_namespace(namespace)
        .map_err(|e| CliError::Crypto(e.to_string()))?;
    for p in pubkey_paths {
        policy = policy
            .add_public_key_file(p)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    for ca in ca_key_paths {
        policy = policy
            .add_ca_file(ca)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    for krl in krl_paths {
        policy = policy
            .add_krl_file(krl)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    if let Some(req) = required_principal {
        policy = policy.with_required_principal(req);
    }
    Ok(policy)
}

/// Verifies `mod_path` against its detached `SSHSIG` `.sig` sidecar (or embedded `.bam_sig`),
/// supporting pinned public keys, CA certificates, optional binary `SSHKRL` revocation lists, and
/// required certificate principals.
pub fn verify_mod_file_with_trust(
    mod_path: &Path,
    sig_path: Option<&Path>,
    pubkey_paths: &[PathBuf],
    ca_key_paths: &[PathBuf],
    krl_paths: &[PathBuf],
    required_principal: Option<&str>,
    namespace: &str,
) -> Result<String, CliError> {
    let policy = build_cli_trust_policy(
        pubkey_paths,
        ca_key_paths,
        krl_paths,
        required_principal,
        namespace,
    )?;

    let binary_bytes = read_file_bounded(mod_path, DEFAULT_MAX_MOD_BINARY_BYTES, "mod binary")?;
    if binary_bytes.is_empty() {
        return Err(CliError::Crypto("mod binary is empty".to_owned()));
    }

    let receipt = match sig_path {
        Some(explicit_sig) => {
            let sig_bytes =
                read_file_bounded(explicit_sig, MAX_SIGNATURE_SIDECAR_BYTES, "signature file")?;
            policy
                .verify_bytes(&binary_bytes, &sig_bytes)
                .map_err(|e| CliError::Crypto(e.to_string()))?
        }
        None => {
            let default_sig = default_sig_path(mod_path);
            if default_sig.is_file() {
                let sig_bytes =
                    read_file_bounded(&default_sig, MAX_SIGNATURE_SIDECAR_BYTES, "signature file")?;
                policy
                    .verify_bytes(&binary_bytes, &sig_bytes)
                    .map_err(|e| CliError::Crypto(e.to_string()))?
            } else {
                policy
                    .verify_embedded_bam_sig(&binary_bytes)
                    .map_err(|e| CliError::Crypto(e.to_string()))?
            }
        }
    };

    Ok(receipt.signer_fingerprint)
}

/// Verifies `mod_path` against its detached `SSHSIG` `.sig` sidecar and pinned OpenSSH Ed25519
/// public keys (`pubkey_paths`), or falls back to embedded `.bam_sig` if `sig_path` is `None` and
/// no sidecar file exists.
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
    let fp =
        verify_mod_file_with_trust(mod_path, sig_path, pubkey_paths, &[], &[], None, namespace)?;
    fp.parse()
        .map_err(|e| CliError::Crypto(format!("invalid signer fingerprint: {e}")))
}
