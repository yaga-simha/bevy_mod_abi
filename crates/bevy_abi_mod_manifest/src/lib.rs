#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Zero-Bevy `mod.toml` manifest parser, SemVer validator, and Kahn topological dependency DAG
//! resolver shared between `bevy_abi_mod_host` and `cargo-bam`.

use bevy_abi_mod_sys::{ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Maximum `mod.toml` file size (`1 MiB`) accepted by [`ModTomlManifest::from_file`].
pub const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;

/// Maximum byte length accepted for a canonical `[mod].id` identifier.
pub const MAX_MOD_ID_LEN: usize = 128;

/// Maximum byte length accepted for a relative POSIX `[mod].entry` / `[mod].library` path.
pub const MAX_ENTRY_PATH_LEN: usize = 1024;

/// Maximum directory nesting depth accepted for a relative POSIX `[mod].entry` path.
pub const MAX_ENTRY_PATH_DEPTH: usize = 32;

/// Returns `true` if `segment` (or its stem before the first `.`) is a Windows reserved DOS device
/// name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, ASCII case-insensitive).
#[must_use]
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

/// Validates a canonical `[mod].id` identifier against path traversal, separators, control bytes,
/// NTFS alternate data streams, and Windows reserved device names.
///
/// Valid IDs are 1..=[`MAX_MOD_ID_LEN`] ASCII bytes matching `[A-Za-z0-9_][A-Za-z0-9_-]*` and must
/// not be a Windows reserved device name (`CON`, `PRN`, `AUX`, `NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`).
pub fn validate_mod_id(id: &str) -> Result<(), String> {
    if id.is_empty() {
        return Err("mod identifier must not be empty".to_owned());
    }
    if id.len() > MAX_MOD_ID_LEN {
        return Err(format!(
            "mod identifier length ({}) exceeds maximum ({MAX_MOD_ID_LEN})",
            id.len()
        ));
    }
    if id.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err("mod identifier must not contain NUL or ASCII control characters".to_owned());
    }
    if id.starts_with('-') {
        return Err("mod identifier must not start with '-'".to_owned());
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err("mod identifier may only contain ASCII letters, digits, '_' or '-'".to_owned());
    }
    if is_windows_reserved_device_name(id) {
        return Err(format!(
            "mod identifier '{id}' is a Windows reserved device name"
        ));
    }
    Ok(())
}

/// Validates a `[mod].entry` / `[mod].library` relative POSIX path against traversal (`..`, `.`),
/// absolute paths, Windows drive/UNC prefixes, NUL/control bytes, NTFS ADS (`:`), trailing dots or
/// spaces, excessive length/depth, and Windows reserved device names.
pub fn validate_manifest_entry_path(entry: &str) -> Result<(), String> {
    if entry.is_empty() {
        return Err("entry path must not be empty".to_owned());
    }
    if entry.len() > MAX_ENTRY_PATH_LEN {
        return Err(format!(
            "entry path length ({}) exceeds maximum ({MAX_ENTRY_PATH_LEN})",
            entry.len()
        ));
    }
    if entry.bytes().any(|b| b < 0x20 || b == 0x7F) {
        return Err("entry path must not contain NUL or ASCII control characters".to_owned());
    }
    if entry.starts_with('/') || entry.ends_with('/') {
        return Err(
            "entry path must be a relative file path without leading or trailing '/'".to_owned(),
        );
    }
    if entry.contains('\\') {
        return Err(
            "entry path must use POSIX '/' separators and must not contain '\\'".to_owned(),
        );
    }
    if entry.contains(':') {
        return Err(
            "entry path must not contain ':' (drive prefixes and NTFS alternate data streams are forbidden)"
                .to_owned(),
        );
    }

    let mut depth = 0usize;
    for seg in entry.split('/') {
        depth += 1;
        if depth > MAX_ENTRY_PATH_DEPTH {
            return Err(format!(
                "entry path exceeds maximum directory depth ({MAX_ENTRY_PATH_DEPTH})"
            ));
        }
        if seg.is_empty() || seg == "." || seg == ".." {
            return Err(format!("entry path contains invalid segment '{seg}'"));
        }
        if seg.ends_with('.') || seg.ends_with(' ') {
            return Err(format!(
                "entry path segment '{seg}' must not end with '.' or space"
            ));
        }
        if is_windows_reserved_device_name(seg) {
            return Err(format!(
                "entry path segment '{seg}' uses a Windows reserved device name"
            ));
        }
    }
    Ok(())
}

fn invalid_manifest_input(path: PathBuf, detail: impl Into<String>) -> ManifestError {
    ManifestError::Io {
        path,
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, detail.into()),
    }
}

fn permission_denied(path: PathBuf, detail: impl Into<String>) -> ManifestError {
    ManifestError::Io {
        path,
        source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, detail.into()),
    }
}

/// Host ABI SemVer version derived from [`bevy_abi_mod_sys::ABI_VERSION`].
#[inline]
#[must_use]
pub fn host_abi_semver() -> semver::Version {
    semver::Version::new(
        u64::from(ABI_VERSION_MAJOR),
        u64::from(ABI_VERSION_MINOR),
        u64::from(ABI_VERSION_PATCH),
    )
}

#[derive(Debug, Deserialize)]
struct RawModToml {
    #[serde(rename = "mod")]
    mod_section: RawModSection,
    #[serde(default)]
    dependencies: BTreeMap<String, String>,
    #[serde(default)]
    optional_dependencies: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct RawModSection {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    version: String,
    #[serde(default)]
    abi_version: Option<String>,
    #[serde(default)]
    entry: Option<String>,
    #[serde(default)]
    library: Option<String>,
    #[serde(default)]
    language: Option<String>,
}

/// Parsed declarative `mod.toml` manifest with validated SemVer version and dependency bounds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModTomlManifest {
    /// Canonical mod identifier (`[mod].id` if present, otherwise `[mod].name`).
    pub name: String,
    /// Optional human-readable display name when both `id` and `name` are provided.
    pub display_name: Option<String>,
    /// Parsed SemVer version from `[mod].version`.
    pub version: semver::Version,
    /// Optional SemVer requirement against the host ABI version (`[mod].abi_version`).
    pub abi_version: Option<semver::VersionReq>,
    /// Optional entry shared-library path (`[mod].entry` or `[mod].library`).
    pub entry: Option<String>,
    /// Optional guest language identifier (`[mod].language`, e.g. `"rust"`, `"c99"`, `"cpp17"`, `"zig"`).
    pub language: Option<String>,
    /// Required dependencies (`<mod_name> = "<semver_req>"`).
    pub dependencies: BTreeMap<String, semver::VersionReq>,
    /// Optional dependencies (`<mod_name> = "<semver_req>"`).
    pub optional_dependencies: BTreeMap<String, semver::VersionReq>,
    /// Filesystem path of the parsed `mod.toml`.
    pub manifest_path: PathBuf,
}

impl ModTomlManifest {
    /// Canonical mod identifier (`[mod].id` if present, otherwise `[mod].name`).
    #[inline]
    #[must_use]
    pub fn id(&self) -> &str {
        &self.name
    }

    /// Parses and SemVer-validates a `mod.toml` document from `toml_str`.
    pub fn parse_str(
        toml_str: &str,
        manifest_path: impl AsRef<Path>,
    ) -> Result<Self, ManifestError> {
        let path = manifest_path.as_ref().to_path_buf();
        let raw: RawModToml =
            toml::from_str(toml_str).map_err(|source| ManifestError::TomlParse {
                path: path.clone(),
                source: Box::new(source),
            })?;

        let normalize_raw_field =
            |opt: Option<String>, label: &str| -> Result<Option<String>, ManifestError> {
                match opt {
                    None => Ok(None),
                    Some(raw_val) => {
                        if raw_val.bytes().any(|b| b < 0x20 || b == 0x7F) {
                            return Err(invalid_manifest_input(
                                path.clone(),
                                format!("{label} must not contain NUL or ASCII control characters"),
                            ));
                        }
                        if raw_val.trim().is_empty() {
                            Ok(None)
                        } else {
                            Ok(Some(raw_val))
                        }
                    }
                }
            };

        let raw_id = normalize_raw_field(raw.mod_section.id, "[mod].id")?;
        let raw_name = normalize_raw_field(raw.mod_section.name, "[mod].name")?;

        let (name, display_name) = match (raw_id, raw_name) {
            (Some(id), Some(name)) => {
                let disp = (id != name).then_some(name.trim().to_string());
                (id, disp)
            }
            (Some(id), None) => (id, None),
            (None, Some(name)) => (name, None),
            (None, None) => return Err(ManifestError::MissingModName { path }),
        };

        if let Err(reason) = validate_mod_id(&name) {
            return Err(invalid_manifest_input(
                path,
                format!("invalid [mod].id '{name}': {reason}"),
            ));
        }

        let version = semver::Version::parse(raw.mod_section.version.trim()).map_err(|source| {
            ManifestError::InvalidVersion {
                path: path.clone(),
                raw: raw.mod_section.version.clone(),
                source,
            }
        })?;

        let abi_version = match raw.mod_section.abi_version {
            Some(raw_abi) => {
                let req = semver::VersionReq::parse(raw_abi.trim()).map_err(|source| {
                    ManifestError::InvalidAbiVersion {
                        path: path.clone(),
                        raw: raw_abi,
                        source,
                    }
                })?;
                Some(req)
            }
            None => None,
        };

        let entry = match raw.mod_section.entry.or(raw.mod_section.library) {
            Some(raw_entry) => {
                if let Err(reason) = validate_manifest_entry_path(&raw_entry) {
                    return Err(invalid_manifest_input(
                        path,
                        format!("invalid [mod].entry '{raw_entry}': {reason}"),
                    ));
                }
                Some(raw_entry)
            }
            None => None,
        };

        let language = raw
            .mod_section
            .language
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut dependencies = BTreeMap::new();
        for (dep, req_str) in raw.dependencies {
            if let Err(reason) = validate_mod_id(&dep) {
                return Err(invalid_manifest_input(
                    path,
                    format!("invalid dependency identifier '{dep}': {reason}"),
                ));
            }
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|source| {
                ManifestError::InvalidDependencyReq {
                    path: path.clone(),
                    mod_name: name.clone(),
                    dependency: dep.clone(),
                    raw: req_str,
                    source,
                }
            })?;
            dependencies.insert(dep, req);
        }

        let mut optional_dependencies = BTreeMap::new();
        for (dep, req_str) in raw.optional_dependencies {
            if let Err(reason) = validate_mod_id(&dep) {
                return Err(invalid_manifest_input(
                    path,
                    format!("invalid optional dependency identifier '{dep}': {reason}"),
                ));
            }
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|source| {
                ManifestError::InvalidDependencyReq {
                    path: path.clone(),
                    mod_name: name.clone(),
                    dependency: dep.clone(),
                    raw: req_str,
                    source,
                }
            })?;
            optional_dependencies.insert(dep, req);
        }

        Ok(Self {
            name,
            display_name,
            version,
            abi_version,
            entry,
            language,
            dependencies,
            optional_dependencies,
            manifest_path: path,
        })
    }

    /// Reads (bounded by [`MAX_MANIFEST_BYTES`]) and parses a `mod.toml` file from disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ManifestError> {
        let p = path.as_ref();
        let file = std::fs::File::open(p).map_err(|source| ManifestError::Io {
            path: p.to_path_buf(),
            source,
        })?;
        let meta = file.metadata().map_err(|source| ManifestError::Io {
            path: p.to_path_buf(),
            source,
        })?;
        if !meta.is_file() {
            return Err(invalid_manifest_input(
                p.to_path_buf(),
                "manifest path is not a regular file",
            ));
        }
        if meta.len() > MAX_MANIFEST_BYTES {
            return Err(invalid_manifest_input(
                p.to_path_buf(),
                format!(
                    "manifest file size ({} bytes) exceeds maximum ({MAX_MANIFEST_BYTES} bytes)",
                    meta.len()
                ),
            ));
        }
        let mut raw = Vec::with_capacity(usize::try_from(meta.len()).unwrap_or(4096));
        file.take(MAX_MANIFEST_BYTES.saturating_add(1))
            .read_to_end(&mut raw)
            .map_err(|source| ManifestError::Io {
                path: p.to_path_buf(),
                source,
            })?;
        if raw.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(invalid_manifest_input(
                p.to_path_buf(),
                format!("manifest file exceeded maximum size ({MAX_MANIFEST_BYTES} bytes)"),
            ));
        }
        let contents = String::from_utf8(raw).map_err(|_| {
            invalid_manifest_input(p.to_path_buf(), "manifest file is not valid UTF-8")
        })?;
        Self::parse_str(&contents, p)
    }

    /// Checks `[mod].abi_version` (when specified) against [`host_abi_semver`].
    pub fn validate_abi_compatibility(&self) -> Result<(), ManifestError> {
        if let Some(ref req) = self.abi_version {
            let host_abi = host_abi_semver();
            if !req.matches(&host_abi) {
                return Err(ManifestError::IncompatibleAbiVersion {
                    path: self.manifest_path.clone(),
                    mod_name: self.name.clone(),
                    required: req.clone(),
                    host_abi,
                });
            }
        }
        Ok(())
    }

    /// Resolves the shared-library path for this manifest relative to `manifest_path.parent()`,
    /// rejecting path traversal, unsafe identifiers, symlinks, and escapes outside the mod root.
    pub fn resolve_library_path(&self) -> Result<PathBuf, ManifestError> {
        if let Err(reason) = validate_mod_id(&self.name) {
            return Err(invalid_manifest_input(
                self.manifest_path.clone(),
                format!("invalid [mod].id '{}': {reason}", self.name),
            ));
        }

        let base_dir = self
            .manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        if let Some(ref entry) = self.entry {
            if let Err(reason) = validate_manifest_entry_path(entry) {
                return Err(invalid_manifest_input(
                    self.manifest_path.clone(),
                    format!("invalid [mod].entry '{entry}': {reason}"),
                ));
            }
            let segments: Vec<&str> = entry.split('/').collect();
            return resolve_confined_regular_file(base_dir, &segments, &self.name);
        }

        let ext = std::env::consts::DLL_EXTENSION;
        let prefix = std::env::consts::DLL_PREFIX;
        let preferred_prefixed_name = format!("{prefix}{}.{ext}", self.name);
        match resolve_confined_regular_file(base_dir, &[&preferred_prefixed_name], &self.name) {
            Ok(p) => return Ok(p),
            Err(ManifestError::MissingLibrary { .. }) => {}
            Err(err) => return Err(err),
        }

        let preferred_plain_name = format!("{}.{ext}", self.name);
        if preferred_plain_name != preferred_prefixed_name {
            match resolve_confined_regular_file(base_dir, &[&preferred_plain_name], &self.name) {
                Ok(p) => return Ok(p),
                Err(ManifestError::MissingLibrary { .. }) => {}
                Err(err) => return Err(err),
            }
        }

        if let Ok(entries) = std::fs::read_dir(base_dir) {
            let mut candidates: Vec<PathBuf> = Vec::new();
            let mut raw_paths: Vec<PathBuf> =
                entries.filter_map(Result::ok).map(|e| e.path()).collect();
            raw_paths.sort();

            for candidate in raw_paths {
                let has_ext = candidate
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e == ext);
                if !has_ext {
                    continue;
                }
                let meta = match std::fs::symlink_metadata(&candidate) {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                if meta.file_type().is_symlink() {
                    return Err(permission_denied(
                        candidate,
                        format!(
                            "mod '{}' library candidate is a symlink (rejected)",
                            self.name
                        ),
                    ));
                }
                if meta.is_file() {
                    verify_canonical_confinement(base_dir, &candidate, &self.name)?;
                    candidates.push(candidate);
                }
            }
            if candidates.len() == 1 {
                return Ok(candidates.remove(0));
            }
        }

        Err(ManifestError::MissingLibrary {
            mod_name: self.name.clone(),
            path: base_dir.join(preferred_prefixed_name),
        })
    }
}

fn verify_canonical_confinement(
    base_dir: &Path,
    candidate: &Path,
    mod_name: &str,
) -> Result<(), ManifestError> {
    let canon_base = std::fs::canonicalize(base_dir).map_err(|source| ManifestError::Io {
        path: base_dir.to_path_buf(),
        source,
    })?;
    let canon_candidate = std::fs::canonicalize(candidate).map_err(|source| ManifestError::Io {
        path: candidate.to_path_buf(),
        source,
    })?;
    if !canon_candidate.starts_with(&canon_base) {
        return Err(permission_denied(
            candidate.to_path_buf(),
            format!(
                "mod '{mod_name}' resolved library '{}' escapes mod root '{}'",
                canon_candidate.display(),
                canon_base.display()
            ),
        ));
    }
    Ok(())
}

fn resolve_confined_regular_file(
    base_dir: &Path,
    segments: &[&str],
    mod_name: &str,
) -> Result<PathBuf, ManifestError> {
    let mut cur = base_dir.to_path_buf();
    let mut last_meta: Option<std::fs::Metadata> = None;
    for seg in segments {
        cur.push(seg);
        match std::fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if meta.file_type().is_symlink() {
                    return Err(permission_denied(
                        cur,
                        format!("mod '{mod_name}' library path traverses or points to a symlink"),
                    ));
                }
                last_meta = Some(meta);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut full = base_dir.to_path_buf();
                for s in segments {
                    full.push(s);
                }
                return Err(ManifestError::MissingLibrary {
                    mod_name: mod_name.to_owned(),
                    path: full,
                });
            }
            Err(source) => {
                return Err(ManifestError::Io { path: cur, source });
            }
        }
    }

    let Some(meta) = last_meta else {
        return Err(invalid_manifest_input(
            base_dir.to_path_buf(),
            format!("mod '{mod_name}' entry path has no segments"),
        ));
    };
    if !meta.is_file() {
        return Err(ManifestError::MissingLibrary {
            mod_name: mod_name.to_owned(),
            path: cur,
        });
    }
    verify_canonical_confinement(base_dir, &cur, mod_name)?;
    Ok(cur)
}

/// Pure manifest parsing, SemVer validation, and dependency DAG diagnostics.
#[derive(Debug)]
pub enum ManifestError {
    /// Filesystem I/O failure reading a directory or `mod.toml`.
    Io {
        /// Path where the I/O operation failed.
        path: PathBuf,
        /// Underlying OS I/O error.
        source: std::io::Error,
    },
    /// TOML syntax or schema failure parsing `mod.toml`.
    TomlParse {
        /// Path to the malformed `mod.toml`.
        path: PathBuf,
        /// Underlying `toml` parse error.
        source: Box<toml::de::Error>,
    },
    /// Neither `[mod].id` nor `[mod].name` was specified (or both were empty).
    MissingModName {
        /// Path to the invalid `mod.toml`.
        path: PathBuf,
    },
    /// `[mod].version` failed SemVer parsing.
    InvalidVersion {
        /// Path to `mod.toml`.
        path: PathBuf,
        /// Raw version string.
        raw: String,
        /// Underlying `semver` error.
        source: semver::Error,
    },
    /// `[mod].abi_version` failed SemVer requirement parsing.
    InvalidAbiVersion {
        /// Path to `mod.toml`.
        path: PathBuf,
        /// Raw ABI version string.
        raw: String,
        /// Underlying `semver` error.
        source: semver::Error,
    },
    /// `[mod].abi_version` does not match the host's ABI version.
    IncompatibleAbiVersion {
        /// Path to `mod.toml`.
        path: PathBuf,
        /// Canonical mod name.
        mod_name: String,
        /// Required host ABI version requirement.
        required: semver::VersionReq,
        /// Host ABI version.
        host_abi: semver::Version,
    },
    /// A dependency entry in `[dependencies]` or `[optional_dependencies]` failed SemVer parsing.
    InvalidDependencyReq {
        /// Path to `mod.toml`.
        path: PathBuf,
        /// Canonical mod name.
        mod_name: String,
        /// Dependency name.
        dependency: String,
        /// Raw SemVer requirement string.
        raw: String,
        /// Underlying `semver` error.
        source: semver::Error,
    },
    /// Two discovered `mod.toml` files declared the same canonical mod name.
    DuplicateModName {
        /// Duplicate mod name.
        mod_name: String,
        /// First manifest path seen.
        first_path: PathBuf,
        /// Conflicting second manifest path.
        second_path: PathBuf,
    },
    /// Expected native shared-library file was not found.
    MissingLibrary {
        /// Canonical mod name.
        mod_name: String,
        /// Expected library path.
        path: PathBuf,
    },
    /// A required dependency is missing (or failed earlier validation/load).
    MissingDependency {
        /// Dependent mod name.
        mod_name: String,
        /// Missing dependency name.
        dependency: String,
        /// Required SemVer constraint.
        required: semver::VersionReq,
    },
    /// A dependency exists but its version does not satisfy the SemVer requirement.
    IncompatibleDependencyVersion {
        /// Dependent mod name.
        mod_name: String,
        /// Dependency name.
        dependency: String,
        /// Required SemVer constraint.
        required: semver::VersionReq,
        /// Version found.
        found: semver::Version,
    },
    /// One or more mods formed (or depended on) a dependency cycle and were quarantined.
    CycleQuarantined {
        /// Sorted list of quarantined mod names involved in or blocked by the cycle.
        cycle_members: Vec<String>,
    },
    /// No `mod.toml` manifests were discovered inside `dir` (for strict CLI directory checks).
    NoManifestsFound {
        /// Directory scanned for `mod.toml` files.
        dir: PathBuf,
    },
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "I/O error reading '{}': {source}", path.display())
            }
            Self::TomlParse { path, source } => {
                write!(f, "failed to parse '{}': {source}", path.display())
            }
            Self::MissingModName { path } => {
                write!(
                    f,
                    "manifest '{}' is missing [mod].id / [mod].name",
                    path.display()
                )
            }
            Self::InvalidVersion { path, raw, source } => {
                write!(
                    f,
                    "manifest '{}' has invalid SemVer version '{raw}': {source}",
                    path.display()
                )
            }
            Self::InvalidAbiVersion { path, raw, source } => {
                write!(
                    f,
                    "manifest '{}' has invalid abi_version '{raw}': {source}",
                    path.display()
                )
            }
            Self::IncompatibleAbiVersion {
                mod_name,
                required,
                host_abi,
                ..
            } => {
                write!(
                    f,
                    "mod '{mod_name}' requires host ABI '{required}', but host ABI is '{host_abi}'"
                )
            }
            Self::InvalidDependencyReq {
                mod_name,
                dependency,
                raw,
                source,
                ..
            } => {
                write!(
                    f,
                    "mod '{mod_name}' has invalid SemVer requirement '{raw}' for dependency '{dependency}': {source}"
                )
            }
            Self::DuplicateModName {
                mod_name,
                first_path,
                second_path,
            } => {
                write!(
                    f,
                    "duplicate mod '{mod_name}' declared in '{}' and '{}'",
                    first_path.display(),
                    second_path.display()
                )
            }
            Self::MissingLibrary { mod_name, path } => {
                write!(
                    f,
                    "mod '{mod_name}' library not found at '{}'",
                    path.display()
                )
            }
            Self::MissingDependency {
                mod_name,
                dependency,
                required,
            } => {
                write!(
                    f,
                    "mod '{mod_name}' is missing required dependency '{dependency}' ({required})"
                )
            }
            Self::IncompatibleDependencyVersion {
                mod_name,
                dependency,
                required,
                found,
            } => {
                write!(
                    f,
                    "mod '{mod_name}' requires '{dependency}' {required}, found {found}"
                )
            }
            Self::CycleQuarantined { cycle_members } => {
                write!(
                    f,
                    "quarantined cyclic dependency subgraph: {}",
                    cycle_members.join(", ")
                )
            }
            Self::NoManifestsFound { dir } => {
                write!(f, "no `mod.toml` manifests found in '{}'", dir.display())
            }
        }
    }
}

impl std::error::Error for ManifestError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::TomlParse { source, .. } => Some(source.as_ref()),
            Self::InvalidVersion { source, .. }
            | Self::InvalidAbiVersion { source, .. }
            | Self::InvalidDependencyReq { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// Result of running SemVer validation and Kahn's topological sort over discovered manifests.
#[derive(Debug)]
pub struct ManifestDagResolution {
    /// Valid, acyclic manifests ordered so every dependency precedes its dependents.
    pub load_order: Vec<ModTomlManifest>,
    /// Sorted canonical names of mods quarantined because they form or depend on a cycle.
    pub quarantined_cycles: Vec<String>,
    /// Deterministic validation, duplicate, SemVer mismatch, and cycle-quarantine diagnostics.
    pub diagnostics: Vec<ManifestError>,
}

/// Resolves SemVer compatibility and runs Kahn's topological sort over `manifests`.
///
/// Any dependency cycle is isolated and recorded in [`ManifestDagResolution::quarantined_cycles`]
/// and [`ManifestDagResolution::diagnostics`] (`CycleQuarantined`) without panicking and without
/// blocking independent acyclic mods from appearing in [`ManifestDagResolution::load_order`].
pub fn resolve_dependency_dag(
    manifests: Vec<ModTomlManifest>,
    already_loaded: &BTreeMap<String, semver::Version>,
) -> ManifestDagResolution {
    let mut diagnostics = Vec::new();
    let mut candidates: BTreeMap<String, ModTomlManifest> = BTreeMap::new();
    let host_abi = host_abi_semver();

    for manifest in manifests {
        if let Some(existing) = candidates.get(&manifest.name) {
            diagnostics.push(ManifestError::DuplicateModName {
                mod_name: manifest.name.clone(),
                first_path: existing.manifest_path.clone(),
                second_path: manifest.manifest_path.clone(),
            });
            continue;
        }
        if let Some(ref abi_req) = manifest.abi_version
            && !abi_req.matches(&host_abi)
        {
            diagnostics.push(ManifestError::IncompatibleAbiVersion {
                path: manifest.manifest_path.clone(),
                mod_name: manifest.name.clone(),
                required: abi_req.clone(),
                host_abi: host_abi.clone(),
            });
            continue;
        }
        candidates.insert(manifest.name.clone(), manifest);
    }

    // Fixed-point validation so missing/incompatible dependencies propagate deterministically.
    loop {
        let mut to_remove: Vec<(String, ManifestError)> = Vec::new();
        for (name, manifest) in &candidates {
            let mut failed_err: Option<ManifestError> = None;

            for (dep, req) in &manifest.dependencies {
                let found_ver = candidates
                    .get(dep)
                    .map(|m| &m.version)
                    .or_else(|| already_loaded.get(dep));
                match found_ver {
                    None => {
                        failed_err = Some(ManifestError::MissingDependency {
                            mod_name: name.clone(),
                            dependency: dep.clone(),
                            required: req.clone(),
                        });
                        break;
                    }
                    Some(ver) if !req.matches(ver) => {
                        failed_err = Some(ManifestError::IncompatibleDependencyVersion {
                            mod_name: name.clone(),
                            dependency: dep.clone(),
                            required: req.clone(),
                            found: ver.clone(),
                        });
                        break;
                    }
                    Some(_) => {}
                }
            }

            if failed_err.is_none() {
                for (opt_dep, req) in &manifest.optional_dependencies {
                    let found_ver = candidates
                        .get(opt_dep)
                        .map(|m| &m.version)
                        .or_else(|| already_loaded.get(opt_dep));
                    if let Some(ver) = found_ver
                        && !req.matches(ver)
                    {
                        failed_err = Some(ManifestError::IncompatibleDependencyVersion {
                            mod_name: name.clone(),
                            dependency: opt_dep.clone(),
                            required: req.clone(),
                            found: ver.clone(),
                        });
                        break;
                    }
                }
            }

            if let Some(err) = failed_err {
                to_remove.push((name.clone(), err));
            }
        }

        if to_remove.is_empty() {
            break;
        }
        for (name, err) in to_remove {
            candidates.remove(&name);
            diagnostics.push(err);
        }
    }

    // Build DAG edges (`dependency -> dependent`) across `candidates`.
    let mut in_degree: BTreeMap<String, usize> =
        candidates.keys().map(|k| (k.clone(), 0usize)).collect();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (name, manifest) in &candidates {
        let mut unique_deps: BTreeSet<&str> = BTreeSet::new();
        for dep in manifest.dependencies.keys() {
            if candidates.contains_key(dep) {
                unique_deps.insert(dep.as_str());
            }
        }
        for opt_dep in manifest.optional_dependencies.keys() {
            if candidates.contains_key(opt_dep) {
                unique_deps.insert(opt_dep.as_str());
            }
        }

        for dep in unique_deps {
            dependents
                .entry(dep.to_string())
                .or_default()
                .insert(name.clone());
            if let Some(deg) = in_degree.get_mut(name) {
                *deg += 1;
            }
        }
    }

    // Kahn's topological sort with deterministic lexicographical ordering.
    let mut ready: BTreeSet<String> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(k, _)| k.clone())
        .collect();

    let mut load_order = Vec::with_capacity(candidates.len());
    while let Some(name) = ready.pop_first() {
        if let Some(manifest) = candidates.get(&name) {
            load_order.push(manifest.clone());
        }
        if let Some(children) = dependents.get(&name) {
            for child in children {
                if let Some(deg) = in_degree.get_mut(child) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        ready.insert(child.clone());
                    }
                }
            }
        }
    }

    let quarantined_cycles: Vec<String> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg > 0)
        .map(|(k, _)| k.clone())
        .collect();

    if !quarantined_cycles.is_empty() {
        diagnostics.push(ManifestError::CycleQuarantined {
            cycle_members: quarantined_cycles.clone(),
        });
    }

    ManifestDagResolution {
        load_order,
        quarantined_cycles,
        diagnostics,
    }
}

/// Discovers `mod.toml` manifests inside `dir` (checking `dir/mod.toml` and `dir/*/mod.toml`).
pub fn discover_mod_manifests(
    dir: &Path,
) -> Result<(Vec<ModTomlManifest>, Vec<ManifestError>), ManifestError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ManifestError::Io {
        path: dir.to_path_buf(),
        source,
    })?;

    let mut manifest_paths = Vec::new();
    let root_manifest = dir.join("mod.toml");
    if root_manifest.is_file() {
        manifest_paths.push(root_manifest);
    }

    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    subdirs.sort();

    for sub in subdirs {
        let candidate = sub.join("mod.toml");
        if candidate.is_file() {
            manifest_paths.push(candidate);
        }
    }

    let mut manifests = Vec::with_capacity(manifest_paths.len());
    let mut errors = Vec::new();
    for path in manifest_paths {
        match ModTomlManifest::from_file(&path) {
            Ok(m) => manifests.push(m),
            Err(e) => errors.push(e),
        }
    }

    Ok((manifests, errors))
}

/// Strict directory validator used by `cargo bam check-mods`: discovers all `mod.toml` files
/// under `dir`, fails on the first parse/ABI/dependency/cycle error or empty directory, and returns
/// the validated manifests in deterministic Kahn topological load order.
pub fn check_mods_dir_strict(dir: &Path) -> Result<Vec<ModTomlManifest>, ManifestError> {
    if !dir.is_dir() {
        return Err(ManifestError::Io {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("mods directory does not exist: {}", dir.display()),
            ),
        });
    }

    let (manifests, parse_errors) = discover_mod_manifests(dir)?;
    if let Some(first_err) = parse_errors.into_iter().next() {
        return Err(first_err);
    }
    if manifests.is_empty() {
        return Err(ManifestError::NoManifestsFound {
            dir: dir.to_path_buf(),
        });
    }

    let resolution = resolve_dependency_dag(manifests, &BTreeMap::new());
    if let Some(first_diag) = resolution.diagnostics.into_iter().next() {
        return Err(first_diag);
    }
    Ok(resolution.load_order)
}

/// Computes the forward topological order `[root_id, ...dependents]` of `root_id` and all
/// transitive active dependents given `(id -> &dependencies)` and `(name -> id)` lookups.
///
/// - For cascading **unload**, iterate the returned slice in **reverse** (`C -> B -> A`).
/// - For cascading **reload**, save & unload in **reverse** (`C -> B -> A`), then reload &
///   restore in **forward** (`A -> B -> C`) order.
pub fn compute_cascading_order<'a, Id, F>(
    root_id: Id,
    active_ids: &BTreeSet<Id>,
    id_by_name: &BTreeMap<String, Id>,
    deps_for_id: F,
) -> Vec<Id>
where
    Id: Ord + Copy,
    F: Fn(Id) -> Option<&'a [String]>,
{
    if deps_for_id(root_id).is_none() {
        return vec![root_id];
    }

    let mut reverse_edges: BTreeMap<Id, BTreeSet<Id>> = BTreeMap::new();
    for &dep_id in active_ids {
        let Some(deps) = deps_for_id(dep_id) else {
            continue;
        };
        for dep_name in deps {
            if let Some(&provider_id) = id_by_name.get(dep_name)
                && active_ids.contains(&provider_id)
            {
                reverse_edges.entry(provider_id).or_default().insert(dep_id);
            }
        }
    }

    let mut affected: BTreeSet<Id> = BTreeSet::new();
    let mut queue = VecDeque::new();
    affected.insert(root_id);
    queue.push_back(root_id);
    while let Some(curr) = queue.pop_front() {
        if let Some(children) = reverse_edges.get(&curr) {
            for &child in children {
                if affected.insert(child) {
                    queue.push_back(child);
                }
            }
        }
    }

    let mut in_degree: BTreeMap<Id, usize> = affected.iter().map(|&id| (id, 0usize)).collect();
    let mut sub_edges: BTreeMap<Id, BTreeSet<Id>> = BTreeMap::new();
    for &id in &affected {
        if let Some(children) = reverse_edges.get(&id) {
            for &child in children {
                if affected.contains(&child)
                    && sub_edges.entry(id).or_default().insert(child)
                    && let Some(deg) = in_degree.get_mut(&child)
                {
                    *deg += 1;
                }
            }
        }
    }

    let mut ready: BTreeSet<Id> = in_degree
        .iter()
        .filter_map(|(&id, &deg)| (deg == 0).then_some(id))
        .collect();
    let mut order = Vec::with_capacity(affected.len());
    while let Some(id) = ready.pop_first() {
        order.push(id);
        if let Some(children) = sub_edges.get(&id) {
            for &child in children {
                if let Some(deg) = in_degree.get_mut(&child) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        ready.insert(child);
                    }
                }
            }
        }
    }

    for id in affected {
        if !order.contains(&id) {
            order.push(id);
        }
    }
    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_and_validate_manifest_fields_and_language() {
        let host = host_abi_semver();
        let valid = format!(
            r#"
[mod]
id = "core_physics"
name = "Core Physics"
version = "1.2.3"
abi_version = "^{host}"
entry = "libcore_physics.so"
language = "rust"

[dependencies]
math = "^1.0"

[optional_dependencies]
debug_draw = ">=0.1.0"
"#
        );
        let parsed = ModTomlManifest::parse_str(&valid, "mods/core/mod.toml").unwrap();
        assert_eq!(parsed.id(), "core_physics");
        assert_eq!(parsed.display_name.as_deref(), Some("Core Physics"));
        assert_eq!(parsed.version, semver::Version::new(1, 2, 3));
        assert_eq!(parsed.language.as_deref(), Some("rust"));
        assert_eq!(parsed.entry.as_deref(), Some("libcore_physics.so"));
        assert!(parsed.validate_abi_compatibility().is_ok());
    }

    #[test]
    fn dag_resolves_order_quarantines_cycles_and_check_strict_rejects_errors() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("base")).unwrap();
        std::fs::create_dir_all(dir.join("app")).unwrap();
        std::fs::create_dir_all(dir.join("cyc_a")).unwrap();
        std::fs::create_dir_all(dir.join("cyc_b")).unwrap();

        let host = host_abi_semver();
        std::fs::write(
            dir.join("base/mod.toml"),
            format!("[mod]\nid = \"base\"\nversion = \"1.0.0\"\nabi_version = \"^{host}\"\n"),
        )
        .unwrap();
        std::fs::write(
            dir.join("app/mod.toml"),
            "[mod]\nid = \"app\"\nversion = \"0.2.0\"\n\n[dependencies]\nbase = \"^1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("cyc_a/mod.toml"),
            "[mod]\nid = \"cyc_a\"\nversion = \"1.0.0\"\n\n[dependencies]\ncyc_b = \"^1.0\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("cyc_b/mod.toml"),
            "[mod]\nid = \"cyc_b\"\nversion = \"1.0.0\"\n\n[dependencies]\ncyc_a = \"^1.0\"\n",
        )
        .unwrap();

        let (manifests, parse_errs) = discover_mod_manifests(dir).unwrap();
        assert!(parse_errs.is_empty());
        let res = resolve_dependency_dag(manifests, &BTreeMap::new());
        let loaded_names: Vec<&str> = res.load_order.iter().map(|m| m.id()).collect();
        assert_eq!(loaded_names, vec!["base", "app"]);
        assert_eq!(res.quarantined_cycles, vec!["cyc_a", "cyc_b"]);

        let strict_err = check_mods_dir_strict(dir).unwrap_err();
        assert!(matches!(strict_err, ManifestError::CycleQuarantined { .. }));
    }
}
