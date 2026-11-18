//! Declarative `mod.toml` manifest parsing and SemVer topological dependency graph resolution.

use bevy_abi_mod_host_sys::{LoadOptions, ModId, ModLoadError};
use bevy_abi_mod_sys::{ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH};
use bevy_ecs::prelude::Resource;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};

/// Host ABI SemVer version derived from [`bevy_abi_mod_sys::ABI_VERSION`].
#[inline]
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
    pub fn id(&self) -> &str {
        &self.name
    }

    /// Parses and SemVer-validates a `mod.toml` document from `toml_str`.
    pub fn parse_str(toml_str: &str, manifest_path: impl AsRef<Path>) -> Result<Self, ModDagError> {
        let path = manifest_path.as_ref().to_path_buf();
        let raw: RawModToml =
            toml::from_str(toml_str).map_err(|source| ModDagError::TomlParse {
                path: path.clone(),
                source: Box::new(source),
            })?;

        let raw_id = raw
            .mod_section
            .id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let raw_name = raw
            .mod_section
            .name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let (name, display_name) = match (raw_id, raw_name) {
            (Some(id), Some(name)) => {
                let disp = (id != name).then_some(name);
                (id, disp)
            }
            (Some(id), None) => (id, None),
            (None, Some(name)) => (name, None),
            (None, None) => return Err(ModDagError::MissingModName { path }),
        };

        let version = semver::Version::parse(raw.mod_section.version.trim()).map_err(|source| {
            ModDagError::InvalidVersion {
                path: path.clone(),
                raw: raw.mod_section.version.clone(),
                source,
            }
        })?;

        let abi_version = match raw.mod_section.abi_version {
            Some(raw_abi) => {
                let req = semver::VersionReq::parse(raw_abi.trim()).map_err(|source| {
                    ModDagError::InvalidAbiVersion {
                        path: path.clone(),
                        raw: raw_abi,
                        source,
                    }
                })?;
                Some(req)
            }
            None => None,
        };

        let entry = raw
            .mod_section
            .entry
            .or(raw.mod_section.library)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let mut dependencies = BTreeMap::new();
        for (dep, req_str) in raw.dependencies {
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|source| {
                ModDagError::InvalidDependencyReq {
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
            let req = semver::VersionReq::parse(req_str.trim()).map_err(|source| {
                ModDagError::InvalidDependencyReq {
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
            dependencies,
            optional_dependencies,
            manifest_path: path,
        })
    }

    /// Reads and parses a `mod.toml` file from disk.
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self, ModDagError> {
        let p = path.as_ref();
        let contents = std::fs::read_to_string(p).map_err(|source| ModDagError::Io {
            path: p.to_path_buf(),
            source,
        })?;
        Self::parse_str(&contents, p)
    }

    /// Resolves the shared-library path for this manifest relative to `manifest_path.parent()`.
    pub fn resolve_library_path(&self) -> Result<PathBuf, ModDagError> {
        let base_dir = self
            .manifest_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));

        if let Some(ref entry) = self.entry {
            let candidate = base_dir.join(entry);
            if candidate.is_file() {
                return Ok(candidate);
            }
            return Err(ModDagError::MissingLibrary {
                mod_name: self.name.clone(),
                path: candidate,
            });
        }

        let ext = std::env::consts::DLL_EXTENSION;
        let prefix = std::env::consts::DLL_PREFIX;
        let preferred_prefixed = base_dir.join(format!("{prefix}{}.{ext}", self.name));
        if preferred_prefixed.is_file() {
            return Ok(preferred_prefixed);
        }
        let preferred_plain = base_dir.join(format!("{}.{ext}", self.name));
        if preferred_plain.is_file() {
            return Ok(preferred_plain);
        }

        if let Ok(entries) = std::fs::read_dir(base_dir) {
            let mut candidates: Vec<PathBuf> = entries
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e == ext)
                })
                .collect();
            candidates.sort();
            if candidates.len() == 1 {
                return Ok(candidates.remove(0));
            }
        }

        Err(ModDagError::MissingLibrary {
            mod_name: self.name.clone(),
            path: preferred_prefixed,
        })
    }
}

/// Errors and deterministic diagnostics produced while parsing `mod.toml` or resolving mod DAGs.
#[derive(Debug)]
pub enum ModDagError {
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
    /// Dynamic shared-library loading failed for a mod in the topological load order.
    LoadFailed {
        /// Canonical mod name.
        mod_name: String,
        /// Library path.
        path: PathBuf,
        /// Underlying [`ModLoadError`].
        source: Box<ModLoadError>,
    },
    /// Target [`ModId`] was not found in [`bevy_abi_mod_host_sys::ModHost`].
    ModNotFound {
        /// Missing mod identifier.
        id: ModId,
    },
}

impl fmt::Display for ModDagError {
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
            Self::LoadFailed {
                mod_name,
                path,
                source,
            } => {
                write!(
                    f,
                    "failed to load mod '{mod_name}' ('{}'): {source}",
                    path.display()
                )
            }
            Self::ModNotFound { id } => write!(f, "mod {id:?} is not loaded"),
        }
    }
}

impl std::error::Error for ModDagError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::TomlParse { source, .. } => Some(source.as_ref()),
            Self::InvalidVersion { source, .. }
            | Self::InvalidAbiVersion { source, .. }
            | Self::InvalidDependencyReq { source, .. } => Some(source),
            Self::LoadFailed { source, .. } => Some(source.as_ref()),
            _ => None,
        }
    }
}

/// Result of running SemVer validation and Kahn's topological sort over discovered manifests.
#[derive(Debug)]
pub struct DagResolution {
    /// Valid, acyclic manifests ordered so every dependency precedes its dependents.
    pub load_order: Vec<ModTomlManifest>,
    /// Sorted canonical names of mods quarantined because they form or depend on a cycle.
    pub quarantined_cycles: Vec<String>,
    /// Deterministic validation, duplicate, SemVer mismatch, and cycle-quarantine diagnostics.
    pub diagnostics: Vec<ModDagError>,
}

/// Resolves SemVer compatibility and runs Kahn's topological sort over `manifests`.
///
/// Any dependency cycle is isolated and recorded in [`DagResolution::quarantined_cycles`] and
/// [`DagResolution::diagnostics`] (`CycleQuarantined`) without panicking and without blocking
/// independent acyclic mods from appearing in [`DagResolution::load_order`].
pub fn resolve_dependency_dag(
    manifests: Vec<ModTomlManifest>,
    already_loaded: &BTreeMap<String, semver::Version>,
) -> DagResolution {
    let mut diagnostics = Vec::new();
    let mut candidates: BTreeMap<String, ModTomlManifest> = BTreeMap::new();
    let host_abi = host_abi_semver();

    for manifest in manifests {
        if let Some(existing) = candidates.get(&manifest.name) {
            diagnostics.push(ModDagError::DuplicateModName {
                mod_name: manifest.name.clone(),
                first_path: existing.manifest_path.clone(),
                second_path: manifest.manifest_path.clone(),
            });
            continue;
        }
        if let Some(ref abi_req) = manifest.abi_version
            && !abi_req.matches(&host_abi)
        {
            diagnostics.push(ModDagError::IncompatibleAbiVersion {
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
        let mut to_remove: Vec<(String, ModDagError)> = Vec::new();
        for (name, manifest) in &candidates {
            let mut failed_err: Option<ModDagError> = None;

            for (dep, req) in &manifest.dependencies {
                let found_ver = candidates
                    .get(dep)
                    .map(|m| &m.version)
                    .or_else(|| already_loaded.get(dep));
                match found_ver {
                    None => {
                        failed_err = Some(ModDagError::MissingDependency {
                            mod_name: name.clone(),
                            dependency: dep.clone(),
                            required: req.clone(),
                        });
                        break;
                    }
                    Some(ver) if !req.matches(ver) => {
                        failed_err = Some(ModDagError::IncompatibleDependencyVersion {
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
                        failed_err = Some(ModDagError::IncompatibleDependencyVersion {
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
        diagnostics.push(ModDagError::CycleQuarantined {
            cycle_members: quarantined_cycles.clone(),
        });
    }

    DagResolution {
        load_order,
        quarantined_cycles,
        diagnostics,
    }
}

/// Discovers `mod.toml` manifests inside `dir` (checking `dir/mod.toml` and `dir/*/mod.toml`).
pub fn discover_mod_manifests(
    dir: &Path,
) -> Result<(Vec<ModTomlManifest>, Vec<ModDagError>), ModDagError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ModDagError::Io {
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

/// Summary report returned by `load_mods_dir` / `load_mods_dir_with`.
#[derive(Debug, Default)]
pub struct DirectoryLoadReport {
    /// Successfully loaded `(mod_name, ModId)` pairs in exact topological load order.
    pub loaded: Vec<(String, ModId)>,
    /// Sorted canonical names of mods quarantined due to dependency cycles.
    pub quarantined_cycles: Vec<String>,
    /// Non-fatal diagnostics (malformed `mod.toml`, missing/incompatible SemVer deps, cycles, or
    /// load failures) recorded while loading the directory.
    pub errors: Vec<ModDagError>,
}

/// Dependency metadata stored per loaded mod inside [`ModDependencyGraph`].
#[derive(Clone, Debug)]
pub struct ModDagNode {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// Canonical mod name.
    pub name: String,
    /// Source shared-library path used to load this mod.
    pub source_path: PathBuf,
    /// Load options used when loading this mod (reused on cascading reload).
    pub options: LoadOptions,
    /// Canonical names of declared dependencies (required + active optional).
    pub dependencies: Vec<String>,
}

/// World [`Resource`] tracking the active mod dependency DAG for reverse-topological cascading
/// `unload_mod` and `reload_mod`.
#[derive(Resource, Default, Debug)]
pub struct ModDependencyGraph {
    nodes_by_id: BTreeMap<ModId, ModDagNode>,
    id_by_name: BTreeMap<String, ModId>,
}

impl ModDependencyGraph {
    /// Creates an empty [`ModDependencyGraph`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a loaded mod node and its dependency names in the DAG.
    pub fn register_loaded_mod(
        &mut self,
        id: ModId,
        name: String,
        source_path: PathBuf,
        options: LoadOptions,
        dependencies: Vec<String>,
    ) {
        if let Some(prev) = self.nodes_by_id.get(&id)
            && prev.name != name
            && self.id_by_name.get(&prev.name) == Some(&id)
        {
            self.id_by_name.remove(&prev.name);
        }
        self.id_by_name.insert(name.clone(), id);
        self.nodes_by_id.insert(
            id,
            ModDagNode {
                id,
                name,
                source_path,
                options,
                dependencies,
            },
        );
    }

    /// Registers an additional dependency edge (`dependent` depends on `dependency_name`) on an
    /// already-tracked loaded mod.
    pub fn register_dependency(&mut self, dependent: ModId, dependency_name: String) {
        if let Some(node) = self.nodes_by_id.get_mut(&dependent)
            && !node.dependencies.iter().any(|d| d == &dependency_name)
        {
            node.dependencies.push(dependency_name);
        }
    }

    /// Borrows the [`ModDagNode`] metadata for `id`, if tracked.
    #[inline]
    pub fn get_node(&self, id: ModId) -> Option<&ModDagNode> {
        self.nodes_by_id.get(&id)
    }

    /// Removes `id` from the dependency graph.
    pub fn unregister_mod(&mut self, id: ModId) {
        if let Some(removed) = self.nodes_by_id.remove(&id)
            && self.id_by_name.get(&removed.name) == Some(&id)
        {
            self.id_by_name.remove(&removed.name);
        }
    }

    /// Clears all tracked mod dependency nodes.
    #[inline]
    pub fn clear(&mut self) {
        self.nodes_by_id.clear();
        self.id_by_name.clear();
    }

    /// Computes the forward topological order `[root_id, ...dependents]` of `root_id` and all
    /// transitive active dependents in `active_ids`.
    ///
    /// - For cascading **unload**, iterate the returned slice in **reverse** (`C -> B -> A`).
    /// - For cascading **reload**, save & unload in **reverse** (`C -> B -> A`), then reload &
    ///   restore in **forward** (`A -> B -> C`) order.
    pub fn cascading_order(&self, root_id: ModId, active_ids: &BTreeSet<ModId>) -> Vec<ModId> {
        if !self.nodes_by_id.contains_key(&root_id) {
            return vec![root_id];
        }

        // Build reverse adjacency (`dependency_id -> set of dependent_ids`) across active nodes.
        let mut reverse_edges: BTreeMap<ModId, BTreeSet<ModId>> = BTreeMap::new();
        for (&dep_id, node) in &self.nodes_by_id {
            if !active_ids.contains(&dep_id) {
                continue;
            }
            for dep_name in &node.dependencies {
                if let Some(&provider_id) = self.id_by_name.get(dep_name)
                    && active_ids.contains(&provider_id)
                {
                    reverse_edges.entry(provider_id).or_default().insert(dep_id);
                }
            }
        }

        // BFS from `root_id` to find all transitively affected dependents.
        let mut affected: BTreeSet<ModId> = BTreeSet::new();
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

        // Kahn's topological sort on the `affected` subgraph.
        let mut in_degree: BTreeMap<ModId, usize> =
            affected.iter().map(|&id| (id, 0usize)).collect();
        let mut sub_edges: BTreeMap<ModId, BTreeSet<ModId>> = BTreeMap::new();
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

        let mut ready: BTreeSet<ModId> = in_degree
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
}
