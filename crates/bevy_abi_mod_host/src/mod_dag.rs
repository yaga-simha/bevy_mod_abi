//! Declarative `mod.toml` manifest parsing and SemVer topological dependency graph resolution,
//! backed by [`bevy_abi_mod_manifest`] with Bevy ECS [`Resource`] integration.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};

use bevy_abi_mod_host_sys::{LoadOptions, ModId, ModLoadError};
use bevy_ecs::prelude::Resource;

pub use bevy_abi_mod_manifest::{
    ManifestError, ModTomlManifest, check_mods_dir_strict, compute_cascading_order, host_abi_semver,
};

/// Errors and deterministic diagnostics produced while parsing `mod.toml`, resolving mod DAGs, or
/// loading discovered native mods in a Bevy `World`.
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

impl From<ManifestError> for ModDagError {
    fn from(err: ManifestError) -> Self {
        match err {
            ManifestError::Io { path, source } => Self::Io { path, source },
            ManifestError::TomlParse { path, source } => Self::TomlParse { path, source },
            ManifestError::MissingModName { path } => Self::MissingModName { path },
            ManifestError::InvalidVersion { path, raw, source } => {
                Self::InvalidVersion { path, raw, source }
            }
            ManifestError::InvalidAbiVersion { path, raw, source } => {
                Self::InvalidAbiVersion { path, raw, source }
            }
            ManifestError::IncompatibleAbiVersion {
                path,
                mod_name,
                required,
                host_abi,
            } => Self::IncompatibleAbiVersion {
                path,
                mod_name,
                required,
                host_abi,
            },
            ManifestError::InvalidDependencyReq {
                path,
                mod_name,
                dependency,
                raw,
                source,
            } => Self::InvalidDependencyReq {
                path,
                mod_name,
                dependency,
                raw,
                source,
            },
            ManifestError::DuplicateModName {
                mod_name,
                first_path,
                second_path,
            } => Self::DuplicateModName {
                mod_name,
                first_path,
                second_path,
            },
            ManifestError::MissingLibrary { mod_name, path } => {
                Self::MissingLibrary { mod_name, path }
            }
            ManifestError::MissingDependency {
                mod_name,
                dependency,
                required,
            } => Self::MissingDependency {
                mod_name,
                dependency,
                required,
            },
            ManifestError::IncompatibleDependencyVersion {
                mod_name,
                dependency,
                required,
                found,
            } => Self::IncompatibleDependencyVersion {
                mod_name,
                dependency,
                required,
                found,
            },
            ManifestError::CycleQuarantined { cycle_members } => {
                Self::CycleQuarantined { cycle_members }
            }
            ManifestError::NoManifestsFound { dir } => Self::Io {
                source: std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("no `mod.toml` manifests found in '{}'", dir.display()),
                ),
                path: dir,
            },
        }
    }
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
                    "failed to load mod '{mod_name}' from '{}': {source}",
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
    let inner = bevy_abi_mod_manifest::resolve_dependency_dag(manifests, already_loaded);
    DagResolution {
        load_order: inner.load_order,
        quarantined_cycles: inner.quarantined_cycles,
        diagnostics: inner.diagnostics.into_iter().map(Into::into).collect(),
    }
}

/// Discovers `mod.toml` manifests inside `dir` (checking `dir/mod.toml` and `dir/*/mod.toml`).
pub fn discover_mod_manifests(
    dir: &Path,
) -> Result<(Vec<ModTomlManifest>, Vec<ModDagError>), ModDagError> {
    let (manifests, errs) = bevy_abi_mod_manifest::discover_mod_manifests(dir)?;
    Ok((manifests, errs.into_iter().map(Into::into).collect()))
}

/// Summary returned by [`ModWorldExt::load_mods_dir`](crate::ModWorldExt::load_mods_dir).
#[derive(Debug, Default)]
pub struct DirectoryLoadReport {
    /// Successfully loaded `(mod_name, ModId)` pairs in exact topological load order.
    pub loaded: Vec<(String, ModId)>,
    /// Sorted canonical names of cyclic mods quarantined before loading.
    pub quarantined_cycles: Vec<String>,
    /// Non-fatal diagnostics (malformed `mod.toml`, missing/incompatible SemVer deps, cycles, or
    /// load failures) encountered while processing independent mods.
    pub errors: Vec<ModDagError>,
}

/// Dependency metadata stored per loaded mod inside [`ModDependencyGraph`].
#[derive(Clone, Debug)]
pub struct ModDagNode {
    /// Live mod identifier.
    pub id: ModId,
    /// Canonical mod name (`LoadedModInfo::name`).
    pub name: String,
    /// Original shared-library path loaded for this mod.
    pub source_path: PathBuf,
    /// `LoadOptions` used when loading this mod (preserved for cascading reload).
    pub options: LoadOptions,
    /// Canonical names of declared active dependencies (`mod_name`s).
    pub dependencies: Vec<String>,
}

/// Runtime Bevy [`Resource`] tracking loaded mod dependency edges for cascading unload and reload.
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
        compute_cascading_order(root_id, active_ids, &self.id_by_name, |id| {
            self.nodes_by_id
                .get(&id)
                .map(|node| node.dependencies.as_slice())
        })
    }
}
