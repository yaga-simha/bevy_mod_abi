//! `mod.toml` SemVer validation, discovery, and Kahn topological sort (`cargo bam check-mods`),
//! delegated to [`bevy_abi_mod_manifest`].

use crate::CliError;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use bevy_abi_mod_manifest::host_abi_semver;

/// Validated `mod.toml` manifest with parsed SemVer fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidatedModManifest {
    /// Canonical mod ID (`[mod].id` or `[mod].name`).
    pub id: String,
    /// Parsed SemVer version.
    pub version: semver::Version,
    /// Optional host ABI SemVer requirement.
    pub abi_version: Option<semver::VersionReq>,
    /// Optional explicit library filename (`[mod].entry` / `[mod].library`).
    pub entry: Option<String>,
    /// Optional guest language (`[mod].language`).
    pub language: Option<String>,
    /// Required SemVer dependencies.
    pub dependencies: BTreeMap<String, semver::VersionReq>,
    /// Optional SemVer dependencies.
    pub optional_dependencies: BTreeMap<String, semver::VersionReq>,
    /// Source `mod.toml` path.
    pub manifest_path: PathBuf,
}

impl From<bevy_abi_mod_manifest::ModTomlManifest> for ValidatedModManifest {
    fn from(m: bevy_abi_mod_manifest::ModTomlManifest) -> Self {
        Self {
            id: m.name,
            version: m.version,
            abi_version: m.abi_version,
            entry: m.entry,
            language: m.language,
            dependencies: m.dependencies,
            optional_dependencies: m.optional_dependencies,
            manifest_path: m.manifest_path,
        }
    }
}

impl ValidatedModManifest {
    /// Parses and SemVer-validates a `mod.toml` file from disk, including host ABI compatibility.
    pub fn from_file(path: &Path) -> Result<Self, CliError> {
        let m = bevy_abi_mod_manifest::ModTomlManifest::from_file(path)
            .map_err(|e| CliError::InvalidManifest(e.to_string()))?;
        m.validate_abi_compatibility()
            .map_err(|e| CliError::InvalidManifest(e.to_string()))?;
        Ok(m.into())
    }

    /// Parses and SemVer-validates a `mod.toml` string, including host ABI compatibility.
    pub fn parse_str(toml_str: &str, path: &Path) -> Result<Self, CliError> {
        let m = bevy_abi_mod_manifest::ModTomlManifest::parse_str(toml_str, path)
            .map_err(|e| CliError::InvalidManifest(e.to_string()))?;
        m.validate_abi_compatibility()
            .map_err(|e| CliError::InvalidManifest(e.to_string()))?;
        Ok(m.into())
    }
}

/// Discovers `dir/mod.toml` and `dir/*/mod.toml`, validates all SemVer constraints, detects
/// dependency cycles via Kahn's algorithm, and returns the topological load order.
pub fn check_mods_dir(dir: &Path) -> Result<Vec<ValidatedModManifest>, CliError> {
    if !dir.is_dir() {
        return Err(CliError::InvalidArgument(format!(
            "mods directory does not exist: {}",
            dir.display()
        )));
    }
    let ordered = bevy_abi_mod_manifest::check_mods_dir_strict(dir).map_err(|e| match e {
        bevy_abi_mod_manifest::ManifestError::CycleQuarantined { cycle_members } => {
            CliError::InvalidManifest(format!(
                "dependency cycle detected among mods: {}",
                cycle_members.join(", ")
            ))
        }
        other => CliError::InvalidManifest(other.to_string()),
    })?;
    Ok(ordered.into_iter().map(Into::into).collect())
}
