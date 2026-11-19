//! Signed `.bampack` mod bundle creation and strict cryptographic verification.
//!
//! Wire Format (all integers little-endian):
//! - Header (`24` bytes): `magic[8]` (`"BAMPACK\0"`), `version: u32` (`1`), `flags: u32` (`0`),
//!   `entry_count: u32`, `reserved: u32` (`0`).
//! - Entries (`0..entry_count`):
//!   - `path_len: u16`, `path_utf8: [u8; path_len]` (normalized relative POSIX path)
//!   - `content_len: u64`, `sha256: [u8; 32]`, `content: [u8; content_len]`
//! - Trailer:
//!   - `sig_len: u32` (`1..=MAX_SIGNATURE_SIDECAR_BYTES`)
//!   - `sig_bytes: [u8; sig_len]` (OpenSSH `SSHSIG` PEM signing bytes `[0..sig_len_offset]`)

use std::collections::BTreeSet;
use std::path::Path;

use sha2::{Digest, Sha256};
use ssh_key::{Certificate, PrivateKey};

use crate::{
    MAX_SIGNATURE_SIDECAR_BYTES, ModSigningError, ModTrustPolicy, SignatureVerificationError,
    VerifiedModSignature, sign_mod_bytes_with_certificate, sign_mod_bytes_with_key,
    sign_mod_bytes_with_openssh_certificate,
};

/// 8-byte `.bampack` magic preamble (`b"BAMPACK\0"`).
pub const BAMPACK_MAGIC: &[u8; 8] = b"BAMPACK\0";

/// Supported `.bampack` format version (`1`).
pub const BAMPACK_VERSION: u32 = 1;

/// Default `SSHSIG` domain-separation namespace for `.bampack` bundles.
pub const DEFAULT_BAMPACK_NAMESPACE: &str = "bevy_abi_mod.pack.v1";

/// Hard upper bound on entries accepted in a single `.bampack` archive.
pub const MAX_BAMPACK_ENTRIES: usize = 4096;

/// Single verified file entry extracted from a `.bampack` archive.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BamPackEntry {
    /// Relative POSIX path inside the bundle (e.g. `"mod.toml"` or `"bin/libmod.dylib"`).
    pub path: String,
    /// Verified SHA-256 digest of [`Self::data`].
    pub sha256: [u8; 32],
    /// Verified payload bytes.
    pub data: Vec<u8>,
}

/// Verified `.bampack` bundle contents paired with the cryptographic [`VerifiedModSignature`] receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBamPack {
    /// Ordered entries extracted from the verified `.bampack` archive.
    pub entries: Vec<BamPackEntry>,
    /// Cryptographic signer, CA, principal, and archive digest receipt.
    pub signature: VerifiedModSignature,
}

fn validate_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("entry path must not be empty".to_owned());
    }
    if path.as_bytes().contains(&0) {
        return Err("entry path must not contain NUL bytes".to_owned());
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err(format!(
            "entry path '{path}' must be relative with POSIX '/' separators"
        ));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(format!("entry path '{path}' must not contain drive prefix"));
    }
    for seg in path.split('/') {
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!(
                "entry path '{path}' contains invalid segment '{seg}'"
            ));
        }
    }
    Ok(())
}

/// Builder producing signed `.bampack` archives.
#[derive(Clone, Debug, Default)]
pub struct BamPackBuilder {
    entries: Vec<BamPackEntry>,
    seen_paths: BTreeSet<String>,
}

impl BamPackBuilder {
    /// Creates an empty [`BamPackBuilder`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a relative path and its payload bytes to the archive, rejecting traversal paths and duplicates.
    pub fn add_entry(
        mut self,
        path: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Result<Self, ModSigningError> {
        let path = path.into();
        validate_entry_path(&path).map_err(|detail| ModSigningError::Encoding { detail })?;
        if self.entries.len() >= MAX_BAMPACK_ENTRIES {
            return Err(ModSigningError::Encoding {
                detail: format!("archive exceeds MAX_BAMPACK_ENTRIES ({MAX_BAMPACK_ENTRIES})"),
            });
        }
        if !self.seen_paths.insert(path.clone()) {
            return Err(ModSigningError::Encoding {
                detail: format!("duplicate archive entry path '{path}'"),
            });
        }
        let data = data.into();
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        self.entries.push(BamPackEntry { path, sha256, data });
        Ok(self)
    }

    fn encode_unsigned_body(&self) -> Result<Vec<u8>, ModSigningError> {
        let entry_count =
            u32::try_from(self.entries.len()).map_err(|_| ModSigningError::Encoding {
                detail: "entry count exceeds u32::MAX".to_owned(),
            })?;
        let mut out = Vec::new();
        out.extend_from_slice(BAMPACK_MAGIC);
        out.extend_from_slice(&BAMPACK_VERSION.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&entry_count.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());

        for entry in &self.entries {
            let path_bytes = entry.path.as_bytes();
            let path_len =
                u16::try_from(path_bytes.len()).map_err(|_| ModSigningError::Encoding {
                    detail: format!("entry path '{}' exceeds 65535 bytes", entry.path),
                })?;
            let content_len = entry.data.len() as u64;
            out.extend_from_slice(&path_len.to_le_bytes());
            out.extend_from_slice(path_bytes);
            out.extend_from_slice(&content_len.to_le_bytes());
            out.extend_from_slice(&entry.sha256);
            out.extend_from_slice(&entry.data);
        }
        Ok(out)
    }

    fn finish_with_sig(mut body: Vec<u8>, sig_pem: &str) -> Result<Vec<u8>, ModSigningError> {
        let sig_bytes = sig_pem.as_bytes();
        let sig_len = u32::try_from(sig_bytes.len()).map_err(|_| ModSigningError::Encoding {
            detail: "signature exceeds u32::MAX".to_owned(),
        })?;
        if u64::from(sig_len) > MAX_SIGNATURE_SIDECAR_BYTES {
            return Err(ModSigningError::Encoding {
                detail: format!(
                    "signature size ({sig_len}) exceeds MAX_SIGNATURE_SIDECAR_BYTES ({MAX_SIGNATURE_SIDECAR_BYTES})"
                ),
            });
        }
        body.extend_from_slice(&sig_len.to_le_bytes());
        body.extend_from_slice(sig_bytes);
        Ok(body)
    }

    /// Serializes and signs the `.bampack` archive with a raw Ed25519 private key.
    pub fn build_signed_with_key(
        &self,
        signing_key: &PrivateKey,
        namespace: &str,
    ) -> Result<Vec<u8>, ModSigningError> {
        let body = self.encode_unsigned_body()?;
        let pem = sign_mod_bytes_with_key(signing_key, namespace, &body)?;
        Self::finish_with_sig(body, &pem)
    }

    /// Serializes and signs the `.bampack` archive with an Ed25519 private key and OpenSSH [`Certificate`].
    pub fn build_signed_with_certificate(
        &self,
        signing_key: &PrivateKey,
        certificate: &Certificate,
        namespace: &str,
    ) -> Result<Vec<u8>, ModSigningError> {
        let body = self.encode_unsigned_body()?;
        let pem = sign_mod_bytes_with_certificate(signing_key, certificate, namespace, &body)?;
        Self::finish_with_sig(body, &pem)
    }

    /// Serializes and signs the `.bampack` archive with an Ed25519 private key and OpenSSH
    /// certificate line (`*-cert.pub`).
    pub fn build_signed_with_openssh_certificate(
        &self,
        signing_key: &PrivateKey,
        certificate_openssh: &str,
        namespace: &str,
    ) -> Result<Vec<u8>, ModSigningError> {
        let body = self.encode_unsigned_body()?;
        let pem = sign_mod_bytes_with_openssh_certificate(
            signing_key,
            certificate_openssh,
            namespace,
            &body,
        )?;
        Self::finish_with_sig(body, &pem)
    }
}

fn malformed(detail: impl Into<String>) -> SignatureVerificationError {
    SignatureVerificationError::MalformedSignature {
        detail: detail.into(),
    }
}

fn read_exact_slice<'a>(
    buf: &'a [u8],
    cursor: &mut usize,
    len: usize,
) -> Result<&'a [u8], SignatureVerificationError> {
    let end = cursor
        .checked_add(len)
        .filter(|&e| e <= buf.len())
        .ok_or_else(|| malformed("truncated .bampack archive"))?;
    let slice = &buf[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn read_u16_le(buf: &[u8], cursor: &mut usize) -> Result<u16, SignatureVerificationError> {
    let s = read_exact_slice(buf, cursor, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_le(buf: &[u8], cursor: &mut usize) -> Result<u32, SignatureVerificationError> {
    let s = read_exact_slice(buf, cursor, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u64_le(buf: &[u8], cursor: &mut usize) -> Result<u64, SignatureVerificationError> {
    let s = read_exact_slice(buf, cursor, 8)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

impl ModTrustPolicy {
    /// Parses and cryptographically verifies a `.bampack` archive in memory against this
    /// policy's trust anchors, revocations, certificate validity policy, and namespace.
    pub fn verify_bampack_bytes(
        &self,
        archive_bytes: &[u8],
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        if !self.has_trust_anchors() {
            return Err(SignatureVerificationError::NoTrustAnchorsConfigured);
        }
        if archive_bytes.is_empty() {
            return Err(SignatureVerificationError::EmptyBinary);
        }
        let archive_len = archive_bytes.len() as u64;
        if archive_len > self.max_binary_bytes() {
            return Err(SignatureVerificationError::BinaryTooLarge {
                size: archive_len,
                limit: self.max_binary_bytes(),
            });
        }

        let mut cursor = 0usize;
        let magic = read_exact_slice(archive_bytes, &mut cursor, BAMPACK_MAGIC.len())?;
        if magic != BAMPACK_MAGIC {
            return Err(malformed("invalid .bampack magic header"));
        }
        let version = read_u32_le(archive_bytes, &mut cursor)?;
        if version != BAMPACK_VERSION {
            return Err(malformed(format!("unsupported .bampack version {version}")));
        }
        let flags = read_u32_le(archive_bytes, &mut cursor)?;
        if flags != 0 {
            return Err(malformed(format!(
                "unsupported non-zero .bampack flags {flags}"
            )));
        }
        let entry_count = read_u32_le(archive_bytes, &mut cursor)? as usize;
        if entry_count > MAX_BAMPACK_ENTRIES {
            return Err(malformed(format!(
                ".bampack entry count {entry_count} exceeds limit ({MAX_BAMPACK_ENTRIES})"
            )));
        }
        let reserved = read_u32_le(archive_bytes, &mut cursor)?;
        if reserved != 0 {
            return Err(malformed(format!(
                "non-zero .bampack reserved header {reserved}"
            )));
        }

        let mut entries = Vec::with_capacity(entry_count.min(64));
        let mut seen_paths = BTreeSet::new();
        for _ in 0..entry_count {
            let path_len = usize::from(read_u16_le(archive_bytes, &mut cursor)?);
            let path_bytes = read_exact_slice(archive_bytes, &mut cursor, path_len)?;
            let path = std::str::from_utf8(path_bytes)
                .map_err(|_| malformed("non-UTF-8 .bampack entry path"))?
                .to_owned();
            validate_entry_path(&path).map_err(malformed)?;
            if !seen_paths.insert(path.clone()) {
                return Err(malformed(format!("duplicate .bampack entry path '{path}'")));
            }

            let content_len_u64 = read_u64_le(archive_bytes, &mut cursor)?;
            let content_len = usize::try_from(content_len_u64)
                .map_err(|_| malformed(".bampack entry content_len overflows usize"))?;
            let expected_sha_slice = read_exact_slice(archive_bytes, &mut cursor, 32)?;
            let mut expected_sha = [0u8; 32];
            expected_sha.copy_from_slice(expected_sha_slice);

            let data_slice = read_exact_slice(archive_bytes, &mut cursor, content_len)?;
            let actual_sha: [u8; 32] = Sha256::digest(data_slice).into();
            if actual_sha != expected_sha {
                return Err(malformed(format!(
                    "SHA-256 mismatch on .bampack entry '{path}'"
                )));
            }

            entries.push(BamPackEntry {
                path,
                sha256: actual_sha,
                data: data_slice.to_vec(),
            });
        }

        let signed_payload_end = cursor;
        let sig_len_u32 = read_u32_le(archive_bytes, &mut cursor)?;
        if sig_len_u32 == 0 {
            return Err(malformed(".bampack signature length must be non-zero"));
        }
        if u64::from(sig_len_u32) > MAX_SIGNATURE_SIDECAR_BYTES {
            return Err(SignatureVerificationError::SignatureFileTooLarge {
                size: u64::from(sig_len_u32),
                limit: MAX_SIGNATURE_SIDECAR_BYTES,
            });
        }
        let sig_bytes = read_exact_slice(archive_bytes, &mut cursor, sig_len_u32 as usize)?;
        if cursor != archive_bytes.len() {
            return Err(malformed(format!(
                "trailing bytes ({}) after .bampack signature trailer",
                archive_bytes.len() - cursor
            )));
        }

        let mut signature = self.verify_bytes(&archive_bytes[..signed_payload_end], sig_bytes)?;
        signature.binary_sha256 = Sha256::digest(archive_bytes).into();
        Ok(VerifiedBamPack { entries, signature })
    }

    /// Reads (bounded by [`Self::max_binary_bytes`]) and cryptographically verifies a `.bampack` file.
    pub fn verify_bampack_file(
        &self,
        path: impl AsRef<Path>,
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        let bytes = crate::read_bounded_binary_bytes(path.as_ref(), self.max_binary_bytes())?;
        self.verify_bampack_bytes(&bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CertificateValidityPolicy;
    use ssh_key::certificate;
    use ssh_key::rand_core::OsRng;
    use ssh_key::{Algorithm, PrivateKey};

    fn make_key() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
    }

    #[test]
    fn bampack_roundtrip_tamper_traversal_and_krl_checks() {
        let ca = make_key();
        let signer = make_key();
        let mut cert_b =
            certificate::Builder::new_with_random_nonce(&mut OsRng, signer.public_key(), 10, 1000)
                .unwrap();
        cert_b.serial(55).unwrap();
        cert_b.key_id("pack-signer").unwrap();
        cert_b.cert_type(certificate::CertType::User).unwrap();
        cert_b.valid_principal("bundle_owner").unwrap();
        let cert = cert_b.sign(&ca).unwrap();

        let pack_bytes = BamPackBuilder::new()
            .add_entry(
                "mod.toml",
                b"[mod]\nname = \"alpha\"\nversion = \"1.0.0\"\n".to_vec(),
            )
            .unwrap()
            .add_entry("lib/libalpha.so", vec![0x7f, b'E', b'L', b'F', 1, 2, 3, 4])
            .unwrap()
            .build_signed_with_certificate(&signer, &cert, DEFAULT_BAMPACK_NAMESPACE)
            .expect("build signed .bampack");

        let policy = ModTrustPolicy::from_ca_openssh(&ca.public_key().to_openssh().unwrap())
            .unwrap()
            .with_namespace(DEFAULT_BAMPACK_NAMESPACE)
            .unwrap()
            .with_validity_policy(CertificateValidityPolicy::FixedTimestamp(100))
            .with_required_principal("bundle_owner");

        let verified = policy
            .verify_bampack_bytes(&pack_bytes)
            .expect("valid .bampack must verify");
        assert_eq!(verified.entries.len(), 2);
        assert_eq!(verified.entries[0].path, "mod.toml");
        assert_eq!(verified.entries[1].path, "lib/libalpha.so");
        assert_eq!(verified.signature.serial, Some(55));

        // Tamper entry byte -> fails SHA-256 and/or signature.
        let mut tampered = pack_bytes.clone();
        tampered[26] ^= 0x01;
        assert!(policy.verify_bampack_bytes(&tampered).is_err());

        // Revoked serial -> rejected.
        let revoked_policy = policy.revoke_serial(55);
        assert!(matches!(
            revoked_policy.verify_bampack_bytes(&pack_bytes),
            Err(SignatureVerificationError::CertificateSerialRevoked { serial: 55 })
        ));

        // Path traversal & duplicate paths rejected at build time.
        assert!(
            BamPackBuilder::new()
                .add_entry("../escape", b"x".to_vec())
                .is_err()
        );
        assert!(
            BamPackBuilder::new()
                .add_entry("/abs/path", b"x".to_vec())
                .is_err()
        );
        assert!(
            BamPackBuilder::new()
                .add_entry("a\\b", b"x".to_vec())
                .is_err()
        );
        assert!(
            BamPackBuilder::new()
                .add_entry("a", b"1".to_vec())
                .unwrap()
                .add_entry("a", b"2".to_vec())
                .is_err()
        );
    }
}
