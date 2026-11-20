//! Signed `.bampack` mod bundle creation, strict cryptographic verification, and hardened disk
//! extraction.
//!
//! Wire Format (all integers little-endian):
//! - Header (`24` bytes): `magic[8]` (`"BAMPACK\0"`), `version: u32` (`1`), `flags: u32` (`0`),
//!   `entry_count: u32` (`1..=MAX_BAMPACK_ENTRIES`), `reserved: u32` (`0`).
//! - Entries (`0..entry_count`):
//!   - `path_len: u16`, `path_utf8: [u8; path_len]` (normalized relative POSIX path)
//!   - `content_len: u64`, `sha256: [u8; 32]`, `content: [u8; content_len]`
//! - Trailer:
//!   - `sig_len: u32` (`1..=MAX_SIGNATURE_SIDECAR_BYTES`)
//!   - `sig_bytes: [u8; sig_len]` (OpenSSH `SSHSIG` PEM signing bytes `[0..sig_len_offset]`)

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use ssh_key::{Certificate, PrivateKey};
use thiserror::Error;

use crate::{
    DEFAULT_MAX_MOD_BINARY_BYTES, MAX_SIGNATURE_SIDECAR_BYTES, ModSigningError, ModTrustPolicy,
    SignatureVerificationError, VerifiedModSignature, sign_mod_bytes_with_certificate,
    sign_mod_bytes_with_key, sign_mod_bytes_with_openssh_certificate,
};

/// 8-byte `.bampack` magic preamble (`b"BAMPACK\0"`).
pub const BAMPACK_MAGIC: &[u8; 8] = b"BAMPACK\0";

/// Supported `.bampack` format version (`1`).
pub const BAMPACK_VERSION: u32 = 1;

/// Default `SSHSIG` domain-separation namespace for `.bampack` bundles.
pub const DEFAULT_BAMPACK_NAMESPACE: &str = "bevy_abi_mod.pack.v1";

/// Hard upper bound on entries accepted in a single `.bampack` archive.
pub const MAX_BAMPACK_ENTRIES: usize = 4096;

/// Maximum UTF-8 byte length permitted for a `.bampack` entry path.
pub const MAX_BAMPACK_PATH_LEN: usize = 1024;

/// Maximum directory nesting depth permitted for a `.bampack` entry path.
pub const MAX_BAMPACK_PATH_DEPTH: usize = 32;

/// Default upper bound (`256 MiB`) on cumulative uncompressed payload bytes inside a `.bampack`.
pub const DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES: u64 = DEFAULT_MAX_MOD_BINARY_BYTES;

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

/// Resource bounds enforced by [`VerifiedBamPack::extract_to_dir_with_limits`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BamPackExtractLimits {
    /// Maximum number of file entries permitted during extraction.
    pub max_entries: usize,
    /// Maximum cumulative uncompressed payload bytes written during extraction.
    pub max_total_bytes: u64,
}

impl Default for BamPackExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: MAX_BAMPACK_ENTRIES,
            max_total_bytes: DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES,
        }
    }
}

/// Errors returned by [`VerifiedBamPack::extract_to_dir`] and
/// [`VerifiedBamPack::extract_to_dir_with_limits`].
#[derive(Debug, Error)]
pub enum BamPackExtractError {
    /// Archive contains zero entries.
    #[error("cannot extract empty .bampack archive (0 entries)")]
    EmptyArchive,

    /// Archive entry count exceeded extraction limit.
    #[error(".bampack entry count ({count}) exceeds extraction limit ({limit})")]
    TooManyEntries {
        /// Entry count in archive.
        count: usize,
        /// Configured limit.
        limit: usize,
    },

    /// Cumulative uncompressed payload bytes exceeded extraction limit.
    #[error(".bampack uncompressed size ({total_bytes} B) exceeds extraction limit ({limit} B)")]
    TotalBytesExceeded {
        /// Cumulative bytes observed (or saturated bound).
        total_bytes: u64,
        /// Configured byte limit.
        limit: u64,
    },

    /// An entry path failed relative POSIX / cross-platform safety validation.
    #[error("unsafe .bampack entry path '{path}': {detail}")]
    InvalidEntryPath {
        /// Rejecting entry path.
        path: String,
        /// Validation failure detail.
        detail: String,
    },

    /// Entries collided case-insensitively or as file-vs-directory prefixes.
    #[error("colliding .bampack entry path '{path}': {detail}")]
    PathCollision {
        /// Colliding entry path.
        path: String,
        /// Collision detail.
        detail: String,
    },

    /// Entry data no longer matched its recorded SHA-256 digest.
    #[error("SHA-256 mismatch while extracting .bampack entry '{path}'")]
    DigestMismatch {
        /// Entry path with mismatched digest.
        path: String,
    },

    /// Destination root or an intermediate/target component was a symbolic link.
    #[error("symlink rejected during .bampack extraction: '{}'", path.display())]
    SymlinkRejected {
        /// Path that was detected as a symlink.
        path: PathBuf,
    },

    /// Canonicalized target path escaped the canonicalized destination directory.
    #[error(
        "extracted path '{}' escapes destination root '{}'",
        path.display(),
        root.display()
    )]
    PathEscape {
        /// Escaping path.
        path: PathBuf,
        /// Destination root directory.
        root: PathBuf,
    },

    /// Refused to overwrite an existing file or non-directory at destination.
    #[error("refusing to overwrite existing path during .bampack extraction: '{}'", path.display())]
    AlreadyExists {
        /// Existing path.
        path: PathBuf,
    },

    /// Underlying filesystem I/O failure during extraction.
    #[error("I/O error extracting '{}': {source}", path.display())]
    Io {
        /// Path where I/O failed.
        path: PathBuf,
        /// Underlying OS I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Verified `.bampack` bundle contents paired with the cryptographic [`VerifiedModSignature`] receipt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedBamPack {
    /// Ordered entries extracted from the verified `.bampack` archive.
    pub entries: Vec<BamPackEntry>,
    /// Cryptographic signer, CA, principal, and archive digest receipt.
    pub signature: VerifiedModSignature,
}

impl VerifiedBamPack {
    /// Safely extracts all verified entries into `dest` using [`BamPackExtractLimits::default`],
    /// rejecting symlinks, overwrites, path traversal, case-insensitive/prefix collisions, and
    /// canonical path escapes. Rolls back all newly created files/directories on any error.
    pub fn extract_to_dir(&self, dest: &Path) -> Result<Vec<PathBuf>, BamPackExtractError> {
        self.extract_to_dir_with_limits(dest, BamPackExtractLimits::default())
    }

    /// Safely extracts all verified entries into `dest` under explicit `limits`, rolling back all
    /// newly created files and directories if any validation or I/O check fails.
    pub fn extract_to_dir_with_limits(
        &self,
        dest: &Path,
        limits: BamPackExtractLimits,
    ) -> Result<Vec<PathBuf>, BamPackExtractError> {
        if self.entries.is_empty() {
            return Err(BamPackExtractError::EmptyArchive);
        }
        if self.entries.len() > limits.max_entries {
            return Err(BamPackExtractError::TooManyEntries {
                count: self.entries.len(),
                limit: limits.max_entries,
            });
        }

        // Pre-validate every entry before touching the filesystem so extraction fails atomically.
        let mut collisions = PathCollisionTracker::default();
        let mut total_bytes: u64 = 0;
        for entry in &self.entries {
            validate_bampack_entry_path(&entry.path).map_err(|detail| {
                BamPackExtractError::InvalidEntryPath {
                    path: entry.path.clone(),
                    detail,
                }
            })?;
            collisions.insert_file(&entry.path).map_err(|detail| {
                BamPackExtractError::PathCollision {
                    path: entry.path.clone(),
                    detail,
                }
            })?;
            total_bytes = total_bytes.checked_add(entry.data.len() as u64).ok_or(
                BamPackExtractError::TotalBytesExceeded {
                    total_bytes: u64::MAX,
                    limit: limits.max_total_bytes,
                },
            )?;
            if total_bytes > limits.max_total_bytes {
                return Err(BamPackExtractError::TotalBytesExceeded {
                    total_bytes,
                    limit: limits.max_total_bytes,
                });
            }
            let actual_sha: [u8; 32] = Sha256::digest(&entry.data).into();
            if actual_sha != entry.sha256 {
                return Err(BamPackExtractError::DigestMismatch {
                    path: entry.path.clone(),
                });
            }
        }

        // Validate destination directory without following a symlinked root.
        match fs::symlink_metadata(dest) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(BamPackExtractError::SymlinkRejected {
                        path: dest.to_path_buf(),
                    });
                }
                if !meta.is_dir() {
                    return Err(BamPackExtractError::AlreadyExists {
                        path: dest.to_path_buf(),
                    });
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                fs::create_dir_all(dest).map_err(|source| BamPackExtractError::Io {
                    path: dest.to_path_buf(),
                    source,
                })?;
                let meta =
                    fs::symlink_metadata(dest).map_err(|source| BamPackExtractError::Io {
                        path: dest.to_path_buf(),
                        source,
                    })?;
                if meta.file_type().is_symlink() {
                    return Err(BamPackExtractError::SymlinkRejected {
                        path: dest.to_path_buf(),
                    });
                }
            }
            Err(source) => {
                return Err(BamPackExtractError::Io {
                    path: dest.to_path_buf(),
                    source,
                });
            }
        }

        let canon_dest = fs::canonicalize(dest).map_err(|source| BamPackExtractError::Io {
            path: dest.to_path_buf(),
            source,
        })?;

        let mut created_files: Vec<PathBuf> = Vec::with_capacity(self.entries.len());
        let mut created_dirs: Vec<PathBuf> = Vec::new();

        let extract_res: Result<(), BamPackExtractError> = (|| {
            for entry in &self.entries {
                let segments: Vec<&str> = entry.path.split('/').collect();
                let (&file_name, parent_segments) =
                    segments
                        .split_last()
                        .ok_or_else(|| BamPackExtractError::InvalidEntryPath {
                            path: entry.path.clone(),
                            detail: "entry path has no filename segment".to_owned(),
                        })?;

                let mut cur_dir = dest.to_path_buf();
                for &seg in parent_segments {
                    cur_dir.push(seg);
                    match fs::symlink_metadata(&cur_dir) {
                        Ok(meta) => {
                            if meta.file_type().is_symlink() {
                                return Err(BamPackExtractError::SymlinkRejected { path: cur_dir });
                            }
                            if !meta.is_dir() {
                                return Err(BamPackExtractError::AlreadyExists { path: cur_dir });
                            }
                        }
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                            fs::create_dir(&cur_dir).map_err(|source| BamPackExtractError::Io {
                                path: cur_dir.clone(),
                                source,
                            })?;
                            created_dirs.push(cur_dir.clone());
                            let meta = fs::symlink_metadata(&cur_dir).map_err(|source| {
                                BamPackExtractError::Io {
                                    path: cur_dir.clone(),
                                    source,
                                }
                            })?;
                            if meta.file_type().is_symlink() {
                                return Err(BamPackExtractError::SymlinkRejected { path: cur_dir });
                            }
                        }
                        Err(source) => {
                            return Err(BamPackExtractError::Io {
                                path: cur_dir,
                                source,
                            });
                        }
                    }

                    let canon_cur =
                        fs::canonicalize(&cur_dir).map_err(|source| BamPackExtractError::Io {
                            path: cur_dir.clone(),
                            source,
                        })?;
                    if !canon_cur.starts_with(&canon_dest) {
                        return Err(BamPackExtractError::PathEscape {
                            path: canon_cur,
                            root: canon_dest.clone(),
                        });
                    }
                }

                let target_path = cur_dir.join(file_name);
                match fs::symlink_metadata(&target_path) {
                    Ok(meta) => {
                        if meta.file_type().is_symlink() {
                            return Err(BamPackExtractError::SymlinkRejected { path: target_path });
                        }
                        return Err(BamPackExtractError::AlreadyExists { path: target_path });
                    }
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(BamPackExtractError::Io {
                            path: target_path,
                            source,
                        });
                    }
                }

                // `create_new(true)` (`O_CREAT | O_EXCL`) atomically refuses to overwrite an
                // existing file or follow a pre-existing symlink raced into place.
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&target_path)
                    .map_err(|source| {
                        if source.kind() == std::io::ErrorKind::AlreadyExists {
                            BamPackExtractError::AlreadyExists {
                                path: target_path.clone(),
                            }
                        } else {
                            BamPackExtractError::Io {
                                path: target_path.clone(),
                                source,
                            }
                        }
                    })?;
                created_files.push(target_path.clone());

                file.write_all(&entry.data)
                    .and_then(|()| file.flush())
                    .map_err(|source| BamPackExtractError::Io {
                        path: target_path.clone(),
                        source,
                    })?;
                drop(file);

                let final_meta = fs::symlink_metadata(&target_path).map_err(|source| {
                    BamPackExtractError::Io {
                        path: target_path.clone(),
                        source,
                    }
                })?;
                if final_meta.file_type().is_symlink() {
                    return Err(BamPackExtractError::SymlinkRejected { path: target_path });
                }
                let canon_target =
                    fs::canonicalize(&target_path).map_err(|source| BamPackExtractError::Io {
                        path: target_path.clone(),
                        source,
                    })?;
                if !canon_target.starts_with(&canon_dest) {
                    return Err(BamPackExtractError::PathEscape {
                        path: canon_target,
                        root: canon_dest.clone(),
                    });
                }
            }
            Ok(())
        })();

        if let Err(err) = extract_res {
            for f in created_files.iter().rev() {
                let _ = fs::remove_file(f);
            }
            for d in created_dirs.iter().rev() {
                let _ = fs::remove_dir(d);
            }
            return Err(err);
        }

        Ok(created_files)
    }
}

fn is_windows_reserved_device_name(segment: &str) -> bool {
    let stem = segment.split('.').next().unwrap_or(segment);
    let bytes = stem.as_bytes();
    match bytes.len() {
        3 => {
            stem.eq_ignore_ascii_case("CON")
                || stem.eq_ignore_ascii_case("PRN")
                || stem.eq_ignore_ascii_case("AUX")
                || stem.eq_ignore_ascii_case("NUL")
        }
        4 => {
            let prefix = &stem[..3];
            let digit = bytes[3];
            (prefix.eq_ignore_ascii_case("COM") || prefix.eq_ignore_ascii_case("LPT"))
                && matches!(digit, b'1'..=b'9')
        }
        _ => false,
    }
}

/// Validates a relative POSIX `.bampack` entry path against path traversal (`..`, `.`), absolute
/// paths, backslashes, Windows drive prefixes and NTFS alternate data streams (`:`), NUL/ASCII
/// control characters, trailing dots/spaces, Windows reserved device names, and excessive
/// path length or directory depth.
pub fn validate_bampack_entry_path(path: &str) -> Result<(), String> {
    if path.is_empty() {
        return Err("entry path must not be empty".to_owned());
    }
    if path.len() > MAX_BAMPACK_PATH_LEN {
        return Err(format!(
            "entry path length ({}) exceeds MAX_BAMPACK_PATH_LEN ({MAX_BAMPACK_PATH_LEN})",
            path.len()
        ));
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err("entry path must not contain NUL or ASCII control characters".to_owned());
    }
    if path.starts_with('/') || path.ends_with('/') || path.contains('\\') {
        return Err(format!(
            "entry path '{path}' must be relative with POSIX '/' separators"
        ));
    }
    if path.contains(':') {
        return Err(format!(
            "entry path '{path}' must not contain ':' (drive prefixes and NTFS ADS are forbidden)"
        ));
    }

    let mut depth = 0usize;
    for seg in path.split('/') {
        depth += 1;
        if depth > MAX_BAMPACK_PATH_DEPTH {
            return Err(format!(
                "entry path '{path}' exceeds MAX_BAMPACK_PATH_DEPTH ({MAX_BAMPACK_PATH_DEPTH})"
            ));
        }
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!(
                "entry path '{path}' contains invalid segment '{seg}'"
            ));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Err(format!(
                "entry path '{path}' contains segment '{seg}' ending with '.' or space"
            ));
        }
        if is_windows_reserved_device_name(seg) {
            return Err(format!(
                "entry path '{path}' contains Windows reserved device name '{seg}'"
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Default)]
struct PathCollisionTracker {
    files_lower: BTreeSet<String>,
    dirs_lower: BTreeSet<String>,
}

impl PathCollisionTracker {
    fn insert_file(&mut self, path: &str) -> Result<(), String> {
        let lower = path.to_lowercase();
        if self.files_lower.contains(&lower) {
            return Err(format!(
                "duplicate or case-insensitive colliding archive entry path '{path}'"
            ));
        }
        if self.dirs_lower.contains(&lower) {
            return Err(format!(
                "archive file entry '{path}' collides with existing directory prefix"
            ));
        }

        // Check every strict ancestor directory prefix for collisions with already-inserted files.
        let mut prefix_end = 0usize;
        while let Some(rel_slash) = lower[prefix_end..].find('/') {
            let slash_idx = prefix_end + rel_slash;
            let ancestor = &lower[..slash_idx];
            if self.files_lower.contains(ancestor) {
                return Err(format!(
                    "archive entry '{path}' directory prefix '{ancestor}' collides with existing file entry"
                ));
            }
            prefix_end = slash_idx + 1;
        }

        // Commit ancestor directory prefixes and file path only after all checks succeed.
        let mut commit_end = 0usize;
        while let Some(rel_slash) = lower[commit_end..].find('/') {
            let slash_idx = commit_end + rel_slash;
            self.dirs_lower.insert(lower[..slash_idx].to_owned());
            commit_end = slash_idx + 1;
        }
        self.files_lower.insert(lower);
        Ok(())
    }
}

/// Builder producing signed `.bampack` archives.
#[derive(Clone, Debug, Default)]
pub struct BamPackBuilder {
    entries: Vec<BamPackEntry>,
    collisions: PathCollisionTracker,
    total_bytes: u64,
}

impl BamPackBuilder {
    /// Creates an empty [`BamPackBuilder`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a relative path and its payload bytes to the archive, rejecting traversal paths,
    /// case-insensitive duplicates, file-vs-directory prefix collisions, and archive bomb limits.
    pub fn add_entry(
        mut self,
        path: impl Into<String>,
        data: impl Into<Vec<u8>>,
    ) -> Result<Self, ModSigningError> {
        let path = path.into();
        validate_bampack_entry_path(&path)
            .map_err(|detail| ModSigningError::Encoding { detail })?;
        if self.entries.len() >= MAX_BAMPACK_ENTRIES {
            return Err(ModSigningError::Encoding {
                detail: format!("archive exceeds MAX_BAMPACK_ENTRIES ({MAX_BAMPACK_ENTRIES})"),
            });
        }
        let data = data.into();
        let next_total = self
            .total_bytes
            .checked_add(data.len() as u64)
            .ok_or_else(|| ModSigningError::Encoding {
                detail: "archive total uncompressed size overflows u64".to_owned(),
            })?;
        if next_total > DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES {
            return Err(ModSigningError::Encoding {
                detail: format!(
                    "archive uncompressed size ({next_total} B) exceeds limit ({DEFAULT_MAX_BAMPACK_UNCOMPRESSED_BYTES} B)"
                ),
            });
        }
        self.collisions
            .insert_file(&path)
            .map_err(|detail| ModSigningError::Encoding { detail })?;
        let sha256: [u8; 32] = Sha256::digest(&data).into();
        self.total_bytes = next_total;
        self.entries.push(BamPackEntry { path, sha256, data });
        Ok(self)
    }

    fn encode_unsigned_body(&self) -> Result<Vec<u8>, ModSigningError> {
        if self.entries.is_empty() {
            return Err(ModSigningError::Encoding {
                detail: "cannot build empty .bampack archive (0 entries)".to_owned(),
            });
        }
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

struct ScannedEntry<'a> {
    path: &'a str,
    expected_sha: [u8; 32],
    data: &'a [u8],
}

struct ScannedBamPack<'a> {
    entries: Vec<ScannedEntry<'a>>,
    signed_payload_end: usize,
    sig_bytes: &'a [u8],
}

/// Zero-copy structural scan of a `.bampack` archive: validates framing, path safety, collision
/// freedom, uncompressed size bounds, and trailer bounds WITHOUT allocating payload copies or
/// computing per-entry SHA-256 digests prior to cryptographic signature verification.
fn scan_bampack_structure<'a>(
    archive_bytes: &'a [u8],
    max_bytes: u64,
    require_signature: bool,
) -> Result<ScannedBamPack<'a>, SignatureVerificationError> {
    if archive_bytes.is_empty() {
        return Err(SignatureVerificationError::EmptyBinary);
    }
    let archive_len = archive_bytes.len() as u64;
    if archive_len > max_bytes {
        return Err(SignatureVerificationError::BinaryTooLarge {
            size: archive_len,
            limit: max_bytes,
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
    if entry_count == 0 {
        return Err(malformed(
            ".bampack archive must contain at least one entry",
        ));
    }
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

    let mut scanned = Vec::with_capacity(entry_count.min(64));
    let mut collisions = PathCollisionTracker::default();
    let mut total_uncompressed: u64 = 0;

    for _ in 0..entry_count {
        let path_len = usize::from(read_u16_le(archive_bytes, &mut cursor)?);
        let path_bytes = read_exact_slice(archive_bytes, &mut cursor, path_len)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| malformed("non-UTF-8 .bampack entry path"))?;
        validate_bampack_entry_path(path).map_err(malformed)?;
        collisions.insert_file(path).map_err(malformed)?;

        let content_len_u64 = read_u64_le(archive_bytes, &mut cursor)?;
        total_uncompressed = total_uncompressed
            .checked_add(content_len_u64)
            .ok_or_else(|| malformed(".bampack cumulative uncompressed size overflows u64"))?;
        if total_uncompressed > max_bytes {
            return Err(SignatureVerificationError::BinaryTooLarge {
                size: total_uncompressed,
                limit: max_bytes,
            });
        }
        let content_len = usize::try_from(content_len_u64)
            .map_err(|_| malformed(".bampack entry content_len overflows usize"))?;
        let expected_sha_slice = read_exact_slice(archive_bytes, &mut cursor, 32)?;
        let mut expected_sha = [0u8; 32];
        expected_sha.copy_from_slice(expected_sha_slice);

        let data = read_exact_slice(archive_bytes, &mut cursor, content_len)?;
        scanned.push(ScannedEntry {
            path,
            expected_sha,
            data,
        });
    }

    let signed_payload_end = cursor;
    let sig_len_u32 = read_u32_le(archive_bytes, &mut cursor)?;
    if require_signature && sig_len_u32 == 0 {
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

    Ok(ScannedBamPack {
        entries: scanned,
        signed_payload_end,
        sig_bytes,
    })
}

fn materialize_verified_entries(
    scanned: Vec<ScannedEntry<'_>>,
) -> Result<Vec<BamPackEntry>, SignatureVerificationError> {
    let mut entries = Vec::with_capacity(scanned.len());
    for item in scanned {
        let actual_sha: [u8; 32] = Sha256::digest(item.data).into();
        if actual_sha != item.expected_sha {
            return Err(malformed(format!(
                "SHA-256 mismatch on .bampack entry '{}'",
                item.path
            )));
        }
        entries.push(BamPackEntry {
            path: item.path.to_owned(),
            sha256: actual_sha,
            data: item.data.to_vec(),
        });
    }
    Ok(entries)
}

/// Parses and structurally/SHA-256 integrity-checks a `.bampack` archive bounded by `max_bytes`
/// without performing cryptographic signature verification.
///
/// Returns `(entries, has_signature_trailer)`. Always prefer [`ModTrustPolicy::verify_bampack_bytes`]
/// when enforcing cryptographic trust.
pub fn parse_bampack_entries_unverified(
    archive_bytes: &[u8],
    max_bytes: u64,
) -> Result<(Vec<BamPackEntry>, bool), SignatureVerificationError> {
    let scanned = scan_bampack_structure(archive_bytes, max_bytes, false)?;
    let has_sig = !scanned.sig_bytes.is_empty();
    let entries = materialize_verified_entries(scanned.entries)?;
    Ok((entries, has_sig))
}

impl ModTrustPolicy {
    /// Parses and cryptographically verifies a `.bampack` archive in memory against this
    /// policy's trust anchors, revocations, certificate validity policy, and namespace.
    ///
    /// Performs a zero-copy structural scan and verifies the Ed25519 signature **before**
    /// computing per-entry SHA-256 digests or allocating entry payload buffers, preventing
    /// unauthenticated 2x RAM / CPU denial-of-service.
    pub fn verify_bampack_bytes(
        &self,
        archive_bytes: &[u8],
    ) -> Result<VerifiedBamPack, SignatureVerificationError> {
        if !self.has_trust_anchors() {
            return Err(SignatureVerificationError::NoTrustAnchorsConfigured);
        }
        let scanned = scan_bampack_structure(archive_bytes, self.max_binary_bytes(), true)?;

        // Verify the cryptographic signature BEFORE hashing or cloning any entry payload bytes.
        let mut signature = self.verify_bytes(
            &archive_bytes[..scanned.signed_payload_end],
            scanned.sig_bytes,
        )?;

        let entries = materialize_verified_entries(scanned.entries)?;
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
