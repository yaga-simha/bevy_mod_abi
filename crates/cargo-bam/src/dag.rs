//! `mod.toml` SemVer validation, discovery, and Kahn topological sort (`cargo bam check-mods`).

use crate::CliError;
use bevy_abi_mod_sys::{ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Host ABI SemVer version derived from `bevy_abi_mod_sys::ABI_VERSION_*`.
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

impl ValidatedModManifest {
    /// Parses and SemVer-validates a `mod.toml` file from disk, including host ABI compatibility.
    pub fn from_file(path: &Path) -> Result<Self, CliError> {
        let content = fs::read_to_string(path)?;
        Self::parse_str(&content, path)
    }

    /// Parses and SemVer-validates a `mod.toml` string, including host ABI compatibility.
    pub fn parse_str(toml_str: &str, path: &Path) -> Result<Self, CliError> {
        let raw: RawModToml = toml::from_str(toml_str)
            .map_err(|e| CliError::InvalidManifest(format!("{}: {e}", path.display())))?;

        let raw_id = raw
            .mod_section
            .id
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let raw_name = raw
            .mod_section
            .name
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        let id = raw_id.or(raw_name).ok_or_else(|| {
            CliError::InvalidManifest(format!(
                "{}: missing [mod].id or [mod].name",
                path.display()
            ))
        })?;

        let version = semver::Version::parse(raw.mod_section.version.trim()).map_err(|e| {
            CliError::InvalidManifest(format!(
                "{}: invalid [mod].version '{}': {e}",
                path.display(),
                raw.mod_section.version
            ))
        })?;

        let abi_version = match raw.mod_section.abi_version {
            Some(raw_abi) => {
                let req = semver::VersionReq::parse(raw_abi.trim()).map_err(|e| {
                    CliError::InvalidManifest(format!(
                        "{}: invalid [mod].abi_version '{raw_abi}': {e}",
                        path.display()
                    ))
                })?;
                let host_abi = host_abi_semver();
                if !req.matches(&host_abi) {
                    return Err(CliError::InvalidManifest(format!(
                        "{}: mod '{id}' requires host ABI '{req}', but host ABI is '{host_abi}'",
                        path.display()
                    )));
                }
                Some(req)
            }
            None => None,
        };

        let entry = raw
            .mod_section
            .entry
            .or(raw.mod_section.library)
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());
        let language = raw
            .mod_section
            .language
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty());

        let mut dependencies = BTreeMap::new();
        for (dep, req_str) in raw.dependencies {
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|e| {
                CliError::InvalidManifest(format!(
                    "{}: mod '{id}' has invalid dependency SemVer '{req_str}' for '{dep}': {e}",
                    path.display()
                ))
            })?;
            dependencies.insert(dep, req);
        }

        let mut optional_dependencies = BTreeMap::new();
        for (dep, req_str) in raw.optional_dependencies {
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|e| {
                CliError::InvalidManifest(format!(
                    "{}: mod '{id}' has invalid optional dependency SemVer '{req_str}' for '{dep}': {e}",
                    path.display()
                ))
            })?;
            optional_dependencies.insert(dep, req);
        }

        Ok(Self {
            id,
            version,
            abi_version,
            entry,
            language,
            dependencies,
            optional_dependencies,
            manifest_path: path.to_path_buf(),
        })
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

    let mut manifest_paths = Vec::new();
    let root_manifest = dir.join("mod.toml");
    if root_manifest.is_file() {
        manifest_paths.push(root_manifest);
    }

    let mut subdirs: Vec<PathBuf> = fs::read_dir(dir)?
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

    if manifest_paths.is_empty() {
        return Err(CliError::InvalidManifest(format!(
            "no `mod.toml` manifests found in {}",
            dir.display()
        )));
    }

    let mut by_id: BTreeMap<String, ValidatedModManifest> = BTreeMap::new();
    for path in manifest_paths {
        let m = ValidatedModManifest::from_file(&path)?;
        if let Some(existing) = by_id.get(&m.id) {
            return Err(CliError::InvalidManifest(format!(
                "duplicate mod id '{}' in {} and {}",
                m.id,
                existing.manifest_path.display(),
                m.manifest_path.display()
            )));
        }
        by_id.insert(m.id.clone(), m);
    }

    // Validate required and optional SemVer constraints.
    for (id, manifest) in &by_id {
        for (dep, req) in &manifest.dependencies {
            let Some(target) = by_id.get(dep) else {
                return Err(CliError::InvalidManifest(format!(
                    "mod '{id}' is missing required dependency '{dep}' ({req})"
                )));
            };
            if !req.matches(&target.version) {
                return Err(CliError::InvalidManifest(format!(
                    "mod '{id}' requires '{dep}' {req}, found {}",
                    target.version
                )));
            }
        }
        for (opt_dep, req) in &manifest.optional_dependencies {
            if let Some(target) = by_id.get(opt_dep)
                && !req.matches(&target.version)
            {
                return Err(CliError::InvalidManifest(format!(
                    "mod '{id}' requires optional '{opt_dep}' {req}, found {}",
                    target.version
                )));
            }
        }
    }

    // Kahn's topological sort (`dependency -> dependent`).
    let mut in_degree: BTreeMap<String, usize> =
        by_id.keys().map(|k| (k.clone(), 0usize)).collect();
    let mut dependents: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for (id, manifest) in &by_id {
        let mut unique_deps: BTreeSet<&str> = BTreeSet::new();
        for dep in manifest.dependencies.keys() {
            unique_deps.insert(dep.as_str());
        }
        for opt_dep in manifest.optional_dependencies.keys() {
            if by_id.contains_key(opt_dep) {
                unique_deps.insert(opt_dep.as_str());
            }
        }
        for dep in unique_deps {
            dependents
                .entry(dep.to_owned())
                .or_default()
                .insert(id.clone());
            if let Some(deg) = in_degree.get_mut(id) {
                *deg += 1;
            }
        }
    }

    let mut ready: BTreeSet<String> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(k, _)| k.clone())
        .collect();
    let mut load_order = Vec::with_capacity(by_id.len());
    while let Some(id) = ready.pop_first() {
        if let Some(m) = by_id.get(&id) {
            load_order.push(m.clone());
        }
        if let Some(children) = dependents.get(&id) {
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

    let cycle_members: Vec<String> = in_degree
        .into_iter()
        .filter_map(|(k, deg)| (deg > 0).then_some(k))
        .collect();
    if !cycle_members.is_empty() {
        return Err(CliError::InvalidManifest(format!(
            "dependency cycle detected among mods: {}",
            cycle_members.join(", ")
        )));
    }

    Ok(load_order)
}
