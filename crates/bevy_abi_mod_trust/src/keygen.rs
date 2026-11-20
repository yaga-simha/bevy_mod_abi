//! Ed25519 OpenSSH keypair generation and cross-platform certificate issuance (`keygen` feature).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use signature::Signer;
use ssh_encoding::{Base64Writer, Encode, LineEnding, Writer};
use ssh_key::certificate::OptionsMap;
use ssh_key::public::KeyData;
use ssh_key::rand_core::{OsRng, RngCore};
use ssh_key::{Algorithm, HashAlg, PrivateKey, PublicKey};

use crate::{
    ED25519_CERT_ALGORITHM_ID, ModSigningError, ModTrustConfigError, validate_strict_ed25519_kd,
};

/// Result of generating an OpenSSH Ed25519 keypair on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GeneratedKeypair {
    /// Path to the generated OpenSSH private key file.
    pub private_key: PathBuf,
    /// Path to the generated OpenSSH `.pub` public key file.
    pub public_key: PathBuf,
    /// OpenSSH SHA-256 fingerprint (`"SHA256:..."`).
    pub fingerprint: String,
}

/// Result of issuing a signed OpenSSH Ed25519 user certificate (`*-cert.pub`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IssuedCertificate {
    /// Complete single-line OpenSSH certificate (`ssh-ed25519-cert-v01@openssh.com ...`).
    pub certificate_openssh: String,
    /// Subject public key SHA-256 fingerprint.
    pub subject_fingerprint: String,
    /// Issuing CA public key SHA-256 fingerprint.
    pub ca_fingerprint: String,
}

fn contains_ascii_control(s: &str) -> bool {
    s.bytes().any(|b| b < 0x20 || b == 0x7F)
}

/// Generates a fresh Ed25519 OpenSSH [`PrivateKey`] in memory.
pub fn generate_ed25519_keypair(comment: Option<&str>) -> Result<PrivateKey, ModSigningError> {
    if let Some(c) = comment
        && contains_ascii_control(c)
    {
        return Err(ModSigningError::Encoding {
            detail: "OpenSSH key comment must not contain NUL or ASCII control characters"
                .to_owned(),
        });
    }
    let mut key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).map_err(|e| {
        ModSigningError::Signing {
            detail: format!("failed to generate Ed25519 keypair: {e}"),
        }
    })?;
    if let Some(c) = comment {
        key.set_comment(c);
    }
    Ok(key)
}

/// Generates an OpenSSH Ed25519 keypair and writes `<out_prefix>` and `<out_prefix>.pub`,
/// refusing to overwrite existing key files.
pub fn write_openssh_keypair(
    out_prefix: &Path,
    comment: Option<&str>,
) -> Result<GeneratedKeypair, ModTrustConfigError> {
    let mut pub_os = out_prefix.as_os_str().to_os_string();
    pub_os.push(".pub");
    let pub_path = PathBuf::from(pub_os);

    for existing in [out_prefix, pub_path.as_path()] {
        if fs::symlink_metadata(existing).is_ok() {
            return Err(ModTrustConfigError::Io {
                path: existing.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "refusing to overwrite existing key file '{}'",
                        existing.display()
                    ),
                ),
            });
        }
    }

    if let Some(parent) = out_prefix.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|source| ModTrustConfigError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    }

    let key = generate_ed25519_keypair(comment).map_err(|e| {
        ModTrustConfigError::WeakOrInvalidPublicKey {
            detail: e.to_string(),
        }
    })?;

    key.write_openssh_file(out_prefix, LineEnding::LF)
        .map_err(|e| ModTrustConfigError::Io {
            path: out_prefix.to_path_buf(),
            source: std::io::Error::other(e.to_string()),
        })?;

    let mut pub_text =
        key.public_key()
            .to_openssh()
            .map_err(|e| ModTrustConfigError::InvalidPublicKey {
                detail: e.to_string(),
            })?;
    pub_text.push('\n');

    let write_pub_res = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&pub_path)
        .and_then(|mut f| f.write_all(pub_text.as_bytes()));
    if let Err(source) = write_pub_res {
        let _ = fs::remove_file(out_prefix);
        return Err(ModTrustConfigError::Io {
            path: pub_path,
            source,
        });
    }

    let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
    Ok(GeneratedKeypair {
        private_key: out_prefix.to_path_buf(),
        public_key: pub_path,
        fingerprint,
    })
}

/// Issues and signs an OpenSSH `ssh-ed25519-cert-v01@openssh.com` user certificate directly in
/// wire format, avoiding platform `std::time::SystemTime` limits (`FILETIME` overflow on Windows
/// for far-future / `u64::MAX` validity windows).
pub fn issue_ed25519_certificate_openssh(
    ca_key: &PrivateKey,
    subject_pubkey: &PublicKey,
    serial: u64,
    key_id: &str,
    principals: &[String],
    valid_after: u64,
    valid_before: u64,
) -> Result<IssuedCertificate, ModSigningError> {
    if ca_key.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: format!("CA key uses '{}'", ca_key.algorithm()),
        });
    }
    if subject_pubkey.algorithm() != Algorithm::Ed25519 {
        return Err(ModSigningError::UnsupportedAlgorithm {
            detail: format!("subject key uses '{}'", subject_pubkey.algorithm()),
        });
    }
    if contains_ascii_control(key_id) {
        return Err(ModSigningError::Encoding {
            detail: "certificate key_id must not contain NUL or ASCII control characters"
                .to_owned(),
        });
    }
    for p in principals {
        if contains_ascii_control(p) {
            return Err(ModSigningError::Encoding {
                detail: "certificate principal must not contain NUL or ASCII control characters"
                    .to_owned(),
            });
        }
    }
    if valid_after >= valid_before {
        return Err(ModSigningError::Encoding {
            detail: format!(
                "invalid certificate validity window ({valid_after} >= {valid_before})"
            ),
        });
    }

    validate_strict_ed25519_kd(ca_key.public_key().key_data())
        .map_err(|detail| ModSigningError::Encoding { detail })?;
    validate_strict_ed25519_kd(subject_pubkey.key_data())
        .map_err(|detail| ModSigningError::Encoding { detail })?;

    let subject_ed = match subject_pubkey.key_data() {
        KeyData::Ed25519(pk) => pk,
        other => {
            return Err(ModSigningError::UnsupportedAlgorithm {
                detail: other.algorithm().to_string(),
            });
        }
    };

    let mut nonce = [0u8; 32];
    OsRng.fill_bytes(&mut nonce);

    let map_enc = |e: ssh_encoding::Error| ModSigningError::Encoding {
        detail: e.to_string(),
    };
    let mut tbs = Vec::new();
    ED25519_CERT_ALGORITHM_ID
        .encode(&mut tbs)
        .map_err(map_enc)?;
    nonce.as_slice().encode(&mut tbs).map_err(map_enc)?;
    subject_ed.encode(&mut tbs).map_err(map_enc)?;
    serial.encode(&mut tbs).map_err(map_enc)?;
    1u32.encode(&mut tbs).map_err(map_enc)?; // CertType::User
    key_id.encode(&mut tbs).map_err(map_enc)?;

    let mut princ_buf = Vec::new();
    for p in principals {
        p.encode(&mut princ_buf).map_err(map_enc)?;
    }
    princ_buf.encode(&mut tbs).map_err(map_enc)?;

    valid_after.encode(&mut tbs).map_err(map_enc)?;
    valid_before.encode(&mut tbs).map_err(map_enc)?;
    OptionsMap::default().encode(&mut tbs).map_err(map_enc)?;
    OptionsMap::default().encode(&mut tbs).map_err(map_enc)?;
    ([] as [u8; 0])
        .as_slice()
        .encode(&mut tbs)
        .map_err(map_enc)?;

    let mut ca_buf = Vec::new();
    ca_key
        .public_key()
        .key_data()
        .encode(&mut ca_buf)
        .map_err(map_enc)?;
    ca_buf.encode(&mut tbs).map_err(map_enc)?;

    let ca_sig = ca_key
        .try_sign(&tbs)
        .map_err(|e| ModSigningError::Signing {
            detail: e.to_string(),
        })?;

    let mut cert_blob = tbs;
    ca_sig
        .encode_prefixed(&mut cert_blob)
        .map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?;

    let mut b64_buf = vec![0u8; cert_blob.len().div_ceil(3) * 4 + 4];
    let b64 = {
        let mut writer =
            Base64Writer::new(&mut b64_buf).map_err(|e| ModSigningError::Encoding {
                detail: e.to_string(),
            })?;
        Writer::write(&mut writer, &cert_blob).map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?;
        writer.finish().map_err(|e| ModSigningError::Encoding {
            detail: e.to_string(),
        })?
    };

    let certificate_openssh = if key_id.trim().is_empty() {
        format!("{ED25519_CERT_ALGORITHM_ID} {b64}\n")
    } else {
        format!("{ED25519_CERT_ALGORITHM_ID} {b64} {key_id}\n")
    };

    Ok(IssuedCertificate {
        certificate_openssh,
        subject_fingerprint: subject_pubkey.fingerprint(HashAlg::Sha256).to_string(),
        ca_fingerprint: ca_key.public_key().fingerprint(HashAlg::Sha256).to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        CertificateValidityPolicy, DEFAULT_SIGNATURE_NAMESPACE, ModTrustPolicy,
        sign_mod_bytes_with_openssh_certificate,
    };

    #[test]
    fn issue_ed25519_cert_supports_i64_max_and_u64_max_on_all_platforms() {
        let ca = generate_ed25519_keypair(Some("ca")).unwrap();
        let signer = generate_ed25519_keypair(Some("signer")).unwrap();

        for max_time in [i64::MAX as u64, u64::MAX] {
            let issued = issue_ed25519_certificate_openssh(
                &ca,
                signer.public_key(),
                77,
                "ci-cert",
                &["sample_mod".to_owned()],
                0,
                max_time,
            )
            .expect("far-future / forever OpenSSH cert must succeed");

            let payload = b"mod-bytes";
            let pem = sign_mod_bytes_with_openssh_certificate(
                &signer,
                &issued.certificate_openssh,
                DEFAULT_SIGNATURE_NAMESPACE,
                payload,
            )
            .expect("sign with issued cert");

            let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
                .unwrap()
                .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(1_700_000_000))
                .with_required_principal("sample_mod");

            let verified = policy
                .verify_bytes(payload, pem.as_bytes())
                .expect("verify with far-future cert");
            assert_eq!(verified.serial, Some(77));
            assert_eq!(verified.key_id.as_deref(), Some("ci-cert"));
            assert_eq!(verified.principals, vec!["sample_mod"]);
        }
    }
}
