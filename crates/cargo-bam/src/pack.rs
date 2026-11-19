//! Deterministic `.bampack` archive creation and cryptographic verification (`cargo bam pack` /
//! `cargo bam verify`), using the canonical wire format verified by [`bevy_abi_mod_trust`].

use crate::builder;
use crate::scaffold::ModLanguage;
use crate::{CliError, ResolvedProject, atomic_write_bytes, dylib_naming, resolve_project_info};
use bevy_abi_mod_trust::{
    BAMPACK_MAGIC, BAMPACK_VERSION, BamPackBuilder, BamPackEntry, HashAlg, MAX_BAMPACK_ENTRIES,
    MAX_SIGNATURE_SIDECAR_BYTES, ModTrustPolicy, PrivateKey, VerifiedBamPack,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Default OpenSSH `SSHSIG` domain-separation namespace for `.bampack` archives.
pub const DEFAULT_BAMPACK_NAMESPACE: &str = bevy_abi_mod_trust::DEFAULT_BAMPACK_NAMESPACE;

/// Configuration for [`pack_mod`] (`cargo bam pack`).
#[derive(Clone, Debug)]
pub struct PackOptions {
    /// Optional explicit path to `mod.toml` or `Cargo.toml`.
    pub manifest_path: Option<PathBuf>,
    /// Optional project directory containing `mod.toml` / `Cargo.toml`.
    pub dir: Option<PathBuf>,
    /// Build (or locate) the release artifact instead of debug.
    pub release: bool,
    /// Optional explicit output `.bampack` file path.
    pub out: Option<PathBuf>,
    /// Optional OpenSSH Ed25519 private key used to sign the `.bampack` archive.
    pub key: Option<PathBuf>,
    /// Optional OpenSSH Ed25519 certificate (`*-cert.pub`) paired with [`Self::key`].
    pub cert: Option<PathBuf>,
    /// `SSHSIG` domain-separation namespace used when signing the archive.
    pub namespace: String,
    /// Optional explicit assets directory to bundle under `assets/`.
    pub assets_dir: Option<PathBuf>,
    /// Skip compiling the mod and locate an existing shared library artifact instead.
    pub skip_build: bool,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            manifest_path: None,
            dir: None,
            release: false,
            out: None,
            key: None,
            cert: None,
            namespace: DEFAULT_BAMPACK_NAMESPACE.to_owned(),
            assets_dir: None,
            skip_build: false,
        }
    }
}

/// Summary returned by [`pack_mod`].
#[derive(Clone, Debug)]
pub struct PackOutput {
    /// Path where the `.bampack` archive was written.
    pub output_path: PathBuf,
    /// Canonical mod identifier from `mod.toml`.
    pub mod_id: String,
    /// Mod SemVer string from `mod.toml`.
    pub version: String,
    /// Detected guest language.
    pub language: ModLanguage,
    /// Relative POSIX path of the bundled shared library inside the archive.
    pub library_entry: String,
    /// Ordered relative POSIX paths of all entries packed in the archive.
    pub entry_paths: Vec<String>,
    /// `true` if the archive was cryptographically signed via `--key`.
    pub signed: bool,
    /// `SSHSIG` namespace used for signing (or configured default when unsigned).
    pub namespace: String,
    /// Signer SHA-256 fingerprint when [`Self::signed`] is `true`.
    pub signer_fingerprint: Option<String>,
    /// Lowercase hex SHA-256 digest of the final `.bampack` file bytes.
    pub archive_sha256: String,
}

fn validate_posix_rel_path(path: &str) -> Result<(), CliError> {
    if path.is_empty() {
        return Err(CliError::InvalidArgument(
            "archive entry path must not be empty".to_owned(),
        ));
    }
    if path.as_bytes().contains(&0) {
        return Err(CliError::InvalidArgument(
            "archive entry path must not contain NUL bytes".to_owned(),
        ));
    }
    if path.starts_with('/') || path.contains('\\') {
        return Err(CliError::InvalidArgument(format!(
            "archive entry path '{path}' must be relative with POSIX '/' separators"
        )));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(CliError::InvalidArgument(format!(
            "archive entry path '{path}' must not contain a drive prefix"
        )));
    }
    for segment in path.split('/') {
        if segment.is_empty() || segment == "." || segment == ".." {
            return Err(CliError::InvalidArgument(format!(
                "archive entry path '{path}' contains invalid path segment '{segment}'"
            )));
        }
    }
    Ok(())
}

fn validate_path_segment(segment: &str) -> Result<(), CliError> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.contains('/')
        || segment.contains('\\')
        || segment.as_bytes().contains(&0)
    {
        return Err(CliError::InvalidArgument(format!(
            "unsafe asset path segment '{segment}'"
        )));
    }
    let bytes = segment.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(CliError::InvalidArgument(format!(
            "unsafe drive-prefixed asset path segment '{segment}'"
        )));
    }
    Ok(())
}

fn collect_assets_recursive(
    current_dir: &Path,
    rel_prefix: &str,
    entries: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), CliError> {
    let mut children: Vec<(String, PathBuf)> = Vec::new();
    for entry_res in fs::read_dir(current_dir)? {
        let entry = entry_res?;
        let os_name = entry.file_name();
        let name = os_name.to_str().ok_or_else(|| {
            CliError::InvalidArgument(format!(
                "non-UTF-8 asset filename under {}",
                current_dir.display()
            ))
        })?;
        validate_path_segment(name)?;
        children.push((name.to_owned(), entry.path()));
    }
    children.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, path) in children {
        let meta = fs::symlink_metadata(&path)?;
        if meta.file_type().is_symlink() {
            return Err(CliError::InvalidArgument(format!(
                "symlink rejected while packing assets: {}",
                path.display()
            )));
        }
        let rel_posix = format!("{rel_prefix}/{name}");
        validate_posix_rel_path(&rel_posix)?;

        if meta.is_dir() {
            collect_assets_recursive(&path, &rel_posix, entries)?;
        } else if meta.is_file() {
            if entries.len() >= MAX_BAMPACK_ENTRIES {
                return Err(CliError::InvalidArgument(format!(
                    "archive exceeds MAX_BAMPACK_ENTRIES ({MAX_BAMPACK_ENTRIES})"
                )));
            }
            let data = fs::read(&path)?;
            if entries.insert(rel_posix.clone(), data).is_some() {
                return Err(CliError::InvalidArgument(format!(
                    "duplicate archive entry path '{rel_posix}'"
                )));
            }
        } else {
            return Err(CliError::InvalidArgument(format!(
                "unsupported non-regular file in assets: {}",
                path.display()
            )));
        }
    }

    Ok(())
}

fn collect_optional_assets(
    project_dir: &Path,
    explicit_assets_dir: Option<&Path>,
    entries: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), CliError> {
    let assets_root = match explicit_assets_dir {
        Some(dir) => {
            let meta = fs::symlink_metadata(dir).map_err(|e| {
                CliError::InvalidArgument(format!(
                    "assets directory {} is not accessible: {e}",
                    dir.display()
                ))
            })?;
            if meta.file_type().is_symlink() {
                return Err(CliError::InvalidArgument(format!(
                    "assets directory must not be a symlink: {}",
                    dir.display()
                )));
            }
            if !meta.is_dir() {
                return Err(CliError::InvalidArgument(format!(
                    "assets path is not a directory: {}",
                    dir.display()
                )));
            }
            Some(dir.to_path_buf())
        }
        None => {
            let default_dir = project_dir.join("assets");
            match fs::symlink_metadata(&default_dir) {
                Ok(meta) => {
                    if meta.file_type().is_symlink() {
                        return Err(CliError::InvalidArgument(format!(
                            "project assets directory must not be a symlink: {}",
                            default_dir.display()
                        )));
                    }
                    if !meta.is_dir() {
                        return Err(CliError::InvalidArgument(format!(
                            "project assets path is not a directory: {}",
                            default_dir.display()
                        )));
                    }
                    Some(default_dir)
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
                Err(err) => return Err(CliError::Io(err)),
            }
        }
    };

    if let Some(root) = assets_root {
        collect_assets_recursive(&root, "assets", entries)?;
    }
    Ok(())
}

fn locate_prebuilt_library(project: &ResolvedProject, release: bool) -> Result<PathBuf, CliError> {
    let (prefix, ext) = dylib_naming();
    let profile_dir = if release { "release" } else { "debug" };
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(manifest) = &project.manifest
        && let Some(entry) = &manifest.entry
    {
        validate_posix_rel_path(entry)?;
        candidates.push(project.project_dir.join(entry));
    }

    let expected_name = format!("{prefix}{}.{ext}", project.lib_stem);
    match project.language {
        ModLanguage::Rust => {
            let target_root = env::var("CARGO_TARGET_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| project.project_dir.join("target"));
            candidates.push(target_root.join(profile_dir).join(&expected_name));
            let mod_id_stem = project.mod_id.replace('-', "_");
            let alt_name = format!("{prefix}{mod_id_stem}.{ext}");
            if alt_name != expected_name {
                candidates.push(target_root.join(profile_dir).join(&alt_name));
            }
        }
        ModLanguage::C99 | ModLanguage::Cpp17 => {
            candidates.push(
                project
                    .project_dir
                    .join("target")
                    .join(profile_dir)
                    .join(&expected_name),
            );
        }
        ModLanguage::Zig => {
            candidates.push(
                project
                    .project_dir
                    .join("zig-out")
                    .join("lib")
                    .join(&expected_name),
            );
            candidates.push(
                project
                    .project_dir
                    .join("zig-out")
                    .join("bin")
                    .join(&expected_name),
            );
        }
    }

    // Fallback for pre-staged mod directories (`<dir>/mod.toml` + `<dir>/<lib>`).
    candidates.push(project.project_dir.join(&expected_name));
    let plain_name = format!("{}.{ext}", project.lib_stem);
    if plain_name != expected_name {
        candidates.push(project.project_dir.join(&plain_name));
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    let searched: Vec<String> = candidates.iter().map(|p| p.display().to_string()).collect();
    Err(CliError::Command(format!(
        "could not locate prebuilt shared library with `--skip-build` (searched: {}; run without `--skip-build` to compile first)",
        searched.join(", ")
    )))
}

fn encode_unsigned_bampack(entries: &BTreeMap<String, Vec<u8>>) -> Result<Vec<u8>, CliError> {
    if entries.len() > MAX_BAMPACK_ENTRIES {
        return Err(CliError::InvalidArgument(format!(
            "archive exceeds MAX_BAMPACK_ENTRIES ({MAX_BAMPACK_ENTRIES})"
        )));
    }
    let entry_count = u32::try_from(entries.len())
        .map_err(|_| CliError::InvalidArgument("entry count exceeds u32::MAX".to_owned()))?;

    let mut out = Vec::new();
    out.extend_from_slice(BAMPACK_MAGIC);
    out.extend_from_slice(&BAMPACK_VERSION.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&entry_count.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    for (path, data) in entries {
        validate_posix_rel_path(path)?;
        let path_bytes = path.as_bytes();
        let path_len = u16::try_from(path_bytes.len()).map_err(|_| {
            CliError::InvalidArgument(format!("entry path '{path}' exceeds 65535 bytes"))
        })?;
        let sha256: [u8; 32] = Sha256::digest(data).into();
        let content_len = data.len() as u64;
        out.extend_from_slice(&path_len.to_le_bytes());
        out.extend_from_slice(path_bytes);
        out.extend_from_slice(&content_len.to_le_bytes());
        out.extend_from_slice(&sha256);
        out.extend_from_slice(data);
    }

    // Unsigned trailer: `sig_len == 0` (rejected by strict cryptographic verifiers).
    out.extend_from_slice(&0u32.to_le_bytes());
    Ok(out)
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Builds (unless `options.skip_build` is set) and packages a guest mod into a deterministic
/// `.bampack` archive, optionally signing it with an Ed25519 key (and optional OpenSSH certificate).
pub fn pack_mod(options: &PackOptions) -> Result<PackOutput, CliError> {
    if options.namespace.is_empty() {
        return Err(CliError::InvalidArgument(
            "`--namespace` must not be empty".to_owned(),
        ));
    }
    if options.cert.is_some() && options.key.is_none() {
        return Err(CliError::InvalidArgument(
            "`--cert` requires `--key <ed25519_key>`".to_owned(),
        ));
    }

    let project = resolve_project_info(options.manifest_path.as_deref(), options.dir.as_deref())?;
    let Some(mod_toml_path) = project.mod_toml_path.as_ref() else {
        return Err(CliError::InvalidManifest(format!(
            "`mod.toml` is required to pack a `.bampack` archive (not found in {})",
            project.project_dir.display()
        )));
    };
    let manifest = project.manifest.as_ref().ok_or_else(|| {
        CliError::InvalidManifest(format!("failed to parse {}", mod_toml_path.display()))
    })?;

    let manifest_meta = fs::symlink_metadata(mod_toml_path)?;
    if manifest_meta.file_type().is_symlink() {
        return Err(CliError::InvalidArgument(format!(
            "`mod.toml` must not be a symlink: {}",
            mod_toml_path.display()
        )));
    }

    let library_path = if options.skip_build {
        locate_prebuilt_library(&project, options.release)?
    } else {
        let built = builder::build_mod(
            options.manifest_path.as_deref(),
            options.dir.as_deref(),
            options.release,
            None,
        )?;
        built.library_path
    };

    let lib_meta = fs::symlink_metadata(&library_path)?;
    if lib_meta.file_type().is_symlink() {
        return Err(CliError::InvalidArgument(format!(
            "built shared library must not be a symlink: {}",
            library_path.display()
        )));
    }

    let lib_bytes = fs::read(&library_path)?;
    if lib_bytes.is_empty() {
        return Err(CliError::InvalidArgument(format!(
            "shared library {} is empty",
            library_path.display()
        )));
    }
    let manifest_bytes = fs::read(mod_toml_path)?;

    let library_entry = if let Some(entry) = &manifest.entry {
        validate_posix_rel_path(entry)?;
        if entry == "mod.toml" || entry.starts_with("assets/") {
            return Err(CliError::InvalidManifest(format!(
                "`[mod].entry` ('{entry}') collides with reserved archive paths"
            )));
        }
        entry.clone()
    } else {
        let file_name = library_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| {
                CliError::InvalidArgument(format!(
                    "shared library path has invalid UTF-8 filename: {}",
                    library_path.display()
                ))
            })?;
        validate_path_segment(file_name)?;
        file_name.to_owned()
    };

    // Deterministic sorted relative POSIX path map across manifest, library, and optional assets.
    let mut entries: BTreeMap<String, Vec<u8>> = BTreeMap::new();
    entries.insert("mod.toml".to_owned(), manifest_bytes);
    if entries.insert(library_entry.clone(), lib_bytes).is_some() {
        return Err(CliError::InvalidManifest(format!(
            "duplicate archive entry path '{library_entry}'"
        )));
    }
    collect_optional_assets(
        &project.project_dir,
        options.assets_dir.as_deref(),
        &mut entries,
    )?;

    let entry_paths: Vec<String> = entries.keys().cloned().collect();

    let (archive_bytes, signed, signer_fingerprint) = if let Some(key_path) = &options.key {
        let signing_key = PrivateKey::read_openssh_file(key_path).map_err(|e| {
            CliError::Crypto(format!(
                "failed to read signing key {}: {e}",
                key_path.display()
            ))
        })?;
        let mut builder = BamPackBuilder::new();
        for (path, data) in &entries {
            builder = builder
                .add_entry(path.clone(), data.clone())
                .map_err(|e| CliError::Crypto(e.to_string()))?;
        }

        let bytes = if let Some(cert_path) = &options.cert {
            let cert_text = fs::read_to_string(cert_path)?;
            builder
                .build_signed_with_openssh_certificate(
                    &signing_key,
                    cert_text.trim(),
                    &options.namespace,
                )
                .map_err(|e| CliError::Crypto(e.to_string()))?
        } else {
            let bytes = builder
                .build_signed_with_key(&signing_key, &options.namespace)
                .map_err(|e| CliError::Crypto(e.to_string()))?;
            // Self-verify freshly produced archive via `bevy_abi_mod_trust`.
            let pub_openssh = signing_key
                .public_key()
                .to_openssh()
                .map_err(|e| CliError::Crypto(e.to_string()))?;
            let policy = ModTrustPolicy::from_public_key_openssh(&pub_openssh)
                .map_err(|e| CliError::Crypto(e.to_string()))?
                .with_namespace(&options.namespace)
                .map_err(|e| CliError::Crypto(e.to_string()))?;
            policy
                .verify_bampack_bytes(&bytes)
                .map_err(|e| CliError::Crypto(format!("self-verification failed: {e}")))?;
            bytes
        };

        let fp = signing_key
            .public_key()
            .fingerprint(HashAlg::Sha256)
            .to_string();
        (bytes, true, Some(fp))
    } else {
        let bytes = encode_unsigned_bampack(&entries)?;
        (bytes, false, None)
    };

    let output_path = match &options.out {
        Some(path) => path.clone(),
        None => {
            let profile_dir = if options.release { "release" } else { "debug" };
            project
                .project_dir
                .join("target")
                .join(profile_dir)
                .join(format!("{}-{}.bampack", manifest.id, manifest.version))
        }
    };
    atomic_write_bytes(&output_path, &archive_bytes)?;

    Ok(PackOutput {
        output_path,
        mod_id: manifest.id.clone(),
        version: manifest.version.to_string(),
        language: project.language,
        library_entry,
        entry_paths,
        signed,
        namespace: options.namespace.clone(),
        signer_fingerprint,
        archive_sha256: sha256_hex(&archive_bytes),
    })
}

/// Returns `true` if `path` has a `.bampack` extension or starts with [`BAMPACK_MAGIC`].
#[must_use]
pub fn is_bampack_file(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bampack"))
    {
        return true;
    }
    let Ok(mut file) = fs::File::open(path) else {
        return false;
    };
    let mut magic = [0u8; 8];
    file.read_exact(&mut magic).is_ok() && &magic == BAMPACK_MAGIC
}

/// Verifies a signed `.bampack` archive file against pinned Ed25519 `.pub` keys and/or CA `.pub`
/// keys using [`bevy_abi_mod_trust::ModTrustPolicy`].
pub fn verify_bampack_file(
    archive_path: &Path,
    pubkey_paths: &[PathBuf],
    ca_key_paths: &[PathBuf],
    required_principal: Option<&str>,
    namespace: &str,
) -> Result<VerifiedBamPack, CliError> {
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
    for pubkey in pubkey_paths {
        policy = policy
            .add_public_key_file(pubkey)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    for ca_key in ca_key_paths {
        policy = policy
            .add_ca_file(ca_key)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    if let Some(principal) = required_principal {
        policy = policy.with_required_principal(principal);
    }

    policy
        .verify_bampack_file(archive_path)
        .map_err(|e| CliError::Crypto(e.to_string()))
}

fn read_exact_slice<'a>(
    buf: &'a [u8],
    cursor: &mut usize,
    len: usize,
) -> Result<&'a [u8], CliError> {
    let end = cursor
        .checked_add(len)
        .filter(|&e| e <= buf.len())
        .ok_or_else(|| CliError::Crypto("truncated .bampack archive".to_owned()))?;
    let slice = &buf[*cursor..end];
    *cursor = end;
    Ok(slice)
}

fn read_u16_le(buf: &[u8], cursor: &mut usize) -> Result<u16, CliError> {
    let s = read_exact_slice(buf, cursor, 2)?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn read_u32_le(buf: &[u8], cursor: &mut usize) -> Result<u32, CliError> {
    let s = read_exact_slice(buf, cursor, 4)?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn read_u64_le(buf: &[u8], cursor: &mut usize) -> Result<u64, CliError> {
    let s = read_exact_slice(buf, cursor, 8)?;
    Ok(u64::from_le_bytes([
        s[0], s[1], s[2], s[3], s[4], s[5], s[6], s[7],
    ]))
}

/// Parses and integrity-checks a `.bampack` archive's framing and per-entry SHA-256 digests,
/// returning `(entries, has_signature_trailer)`.
///
/// This does **not** perform cryptographic signature verification; use [`verify_bampack_file`] or
/// [`ModTrustPolicy::verify_bampack_bytes`] for trust verification.
pub fn inspect_bampack_entries(
    archive_bytes: &[u8],
) -> Result<(Vec<BamPackEntry>, bool), CliError> {
    let mut cursor = 0usize;
    let magic = read_exact_slice(archive_bytes, &mut cursor, BAMPACK_MAGIC.len())?;
    if magic != BAMPACK_MAGIC {
        return Err(CliError::Crypto("invalid .bampack magic header".to_owned()));
    }
    let version = read_u32_le(archive_bytes, &mut cursor)?;
    if version != BAMPACK_VERSION {
        return Err(CliError::Crypto(format!(
            "unsupported .bampack version {version}"
        )));
    }
    let flags = read_u32_le(archive_bytes, &mut cursor)?;
    if flags != 0 {
        return Err(CliError::Crypto(format!(
            "unsupported non-zero .bampack flags {flags}"
        )));
    }
    let entry_count = read_u32_le(archive_bytes, &mut cursor)? as usize;
    if entry_count > MAX_BAMPACK_ENTRIES {
        return Err(CliError::Crypto(format!(
            ".bampack entry count {entry_count} exceeds limit ({MAX_BAMPACK_ENTRIES})"
        )));
    }
    let reserved = read_u32_le(archive_bytes, &mut cursor)?;
    if reserved != 0 {
        return Err(CliError::Crypto(format!(
            "non-zero .bampack reserved header {reserved}"
        )));
    }

    let mut entries = Vec::with_capacity(entry_count.min(64));
    let mut seen_paths = BTreeSet::new();
    for _ in 0..entry_count {
        let path_len = usize::from(read_u16_le(archive_bytes, &mut cursor)?);
        let path_bytes = read_exact_slice(archive_bytes, &mut cursor, path_len)?;
        let path = std::str::from_utf8(path_bytes)
            .map_err(|_| CliError::Crypto("non-UTF-8 .bampack entry path".to_owned()))?
            .to_owned();
        validate_posix_rel_path(&path)?;
        if !seen_paths.insert(path.clone()) {
            return Err(CliError::Crypto(format!(
                "duplicate .bampack entry path '{path}'"
            )));
        }

        let content_len_u64 = read_u64_le(archive_bytes, &mut cursor)?;
        let content_len = usize::try_from(content_len_u64).map_err(|_| {
            CliError::Crypto(".bampack entry content_len overflows usize".to_owned())
        })?;
        let expected_sha_slice = read_exact_slice(archive_bytes, &mut cursor, 32)?;
        let mut expected_sha = [0u8; 32];
        expected_sha.copy_from_slice(expected_sha_slice);

        let data_slice = read_exact_slice(archive_bytes, &mut cursor, content_len)?;
        let actual_sha: [u8; 32] = Sha256::digest(data_slice).into();
        if actual_sha != expected_sha {
            return Err(CliError::Crypto(format!(
                "SHA-256 mismatch on .bampack entry '{path}'"
            )));
        }

        entries.push(BamPackEntry {
            path,
            sha256: actual_sha,
            data: data_slice.to_vec(),
        });
    }

    let sig_len = read_u32_le(archive_bytes, &mut cursor)?;
    if u64::from(sig_len) > MAX_SIGNATURE_SIDECAR_BYTES {
        return Err(CliError::Crypto(format!(
            ".bampack signature trailer ({sig_len} B) exceeds limit ({MAX_SIGNATURE_SIDECAR_BYTES} B)"
        )));
    }
    let _sig_bytes = read_exact_slice(archive_bytes, &mut cursor, sig_len as usize)?;
    if cursor != archive_bytes.len() {
        return Err(CliError::Crypto(format!(
            "trailing bytes ({}) after .bampack signature trailer",
            archive_bytes.len() - cursor
        )));
    }

    Ok((entries, sig_len > 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_trust::generate_ed25519_keypair;
    use tempfile::TempDir;

    #[test]
    fn unsigned_wire_prefix_matches_bampack_builder_exactly() {
        let mut entries = BTreeMap::new();
        entries.insert("assets/config.json".to_owned(), b"{\"ok\":true}".to_vec());
        entries.insert("libdemo.so".to_owned(), vec![1, 2, 3, 4, 5]);
        entries.insert(
            "mod.toml".to_owned(),
            b"[mod]\nid = \"demo\"\nversion = \"1.0.0\"\n".to_vec(),
        );

        let unsigned = encode_unsigned_bampack(&entries).unwrap();
        let (parsed_entries, has_sig) = inspect_bampack_entries(&unsigned).unwrap();
        assert!(!has_sig);
        assert_eq!(parsed_entries.len(), 3);
        assert_eq!(parsed_entries[0].path, "assets/config.json");
        assert_eq!(parsed_entries[1].path, "libdemo.so");
        assert_eq!(parsed_entries[2].path, "mod.toml");

        let key = generate_ed25519_keypair(Some("unit")).unwrap();
        let mut builder = BamPackBuilder::new();
        for (p, data) in &entries {
            builder = builder.add_entry(p.clone(), data.clone()).unwrap();
        }
        let signed = builder
            .build_signed_with_key(&key, DEFAULT_BAMPACK_NAMESPACE)
            .unwrap();

        // Everything before the 4-byte `sig_len` trailer must match byte-for-byte.
        let unsigned_body_len = unsigned.len() - 4;
        assert_eq!(&unsigned[..unsigned_body_len], &signed[..unsigned_body_len]);
        assert_eq!(&unsigned[unsigned_body_len..], &0u32.to_le_bytes());

        let (_, signed_has_sig) = inspect_bampack_entries(&signed).unwrap();
        assert!(signed_has_sig);
    }

    #[test]
    fn rejects_path_traversal_and_unsafe_segments() {
        assert!(validate_posix_rel_path("../escape").is_err());
        assert!(validate_posix_rel_path("assets/../secret").is_err());
        assert!(validate_posix_rel_path("/abs/path").is_err());
        assert!(validate_posix_rel_path("assets\\win").is_err());
        assert!(validate_posix_rel_path("C:evil").is_err());
        assert!(validate_posix_rel_path("assets/ok/file.png").is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn collect_assets_rejects_symlinks() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let assets = tmp.path().join("assets");
        fs::create_dir_all(&assets).unwrap();
        let secret = tmp.path().join("secret.txt");
        fs::write(&secret, b"do-not-leak").unwrap();
        symlink(&secret, assets.join("link.txt")).unwrap();

        let mut entries = BTreeMap::new();
        let err = collect_optional_assets(tmp.path(), None, &mut entries).unwrap_err();
        assert!(
            err.to_string().contains("symlink"),
            "expected symlink rejection error, got: {err}"
        );
    }

    #[test]
    fn cert_without_key_is_rejected() {
        let opts = PackOptions {
            cert: Some(PathBuf::from("signer-cert.pub")),
            ..PackOptions::default()
        };
        let err = pack_mod(&opts).unwrap_err();
        assert!(matches!(err, CliError::InvalidArgument(_)));
    }
}
