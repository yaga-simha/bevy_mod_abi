//! Typed diagnostic errors (`thiserror`) for native shared-library mod loading and unloading.

use crate::loader::ModId;
use crate::registry::ComponentCollisionError;
use bevy_abi_mod_sys::{FfiResult, ScheduleId};
use std::path::PathBuf;
use thiserror::Error;

/// Checks SemVer compatibility between packed `(major << 24) | (minor << 16) | patch` ABI versions.
///
/// Delegates directly to [`bevy_abi_mod_sys::abi_version_is_compatible`].
#[inline]
pub const fn is_abi_version_compatible(host_packed: u32, guest_packed: u32) -> bool {
    bevy_abi_mod_sys::abi_version_is_compatible(host_packed, guest_packed)
}

/// Typed diagnostic error returned when loading, validating, scheduling, or unloading a native mod fails.
#[derive(Debug, Error)]
pub enum ModLoadError {
    /// Failed to shadow-copy the shared library prior to `dlopen`.
    #[error("failed to shadow-copy mod '{path}': {source}")]
    ShadowCopy {
        /// Source or destination path involved in the copy failure.
        path: PathBuf,
        /// Underlying filesystem I/O error.
        #[source]
        source: std::io::Error,
    },

    /// Cryptographic signature or CA certificate verification failed prior to execution.
    #[cfg(feature = "signature-verification")]
    #[error(
        "signature verification failed for mod '{path}' (signature '{signature_path}'): {source}"
    )]
    SignatureVerification {
        /// Source mod shared library path.
        path: PathBuf,
        /// Detached `.sig` sidecar path evaluated for this mod.
        signature_path: PathBuf,
        /// Structured cryptographic or policy verification failure.
        #[source]
        source: crate::loader::signing::SignatureVerificationError,
    },

    /// Failed to open the shared library via OS dynamic linker (`dlopen` / `LoadLibrary`).
    #[error("failed to load mod '{path}': {source}")]
    DlOpen {
        /// Path passed to the loader.
        path: PathBuf,
        /// Underlying `libloading` OS linker error.
        #[source]
        source: libloading::Error,
    },

    /// Shared library did not export the mandatory `bevy_abi_mod_manifest` symbol.
    #[error("mod '{path}' is missing mandatory symbol 'bevy_abi_mod_manifest': {source}")]
    MissingManifest {
        /// Path of the loaded library.
        path: PathBuf,
        /// Underlying symbol lookup error.
        #[source]
        source: libloading::Error,
    },

    /// `bevy_abi_mod_manifest` returned null, malformed UTF-8, empty metadata, or invalid `struct_size`.
    #[error("mod '{path}' returned an invalid BamManifest ({status}): {detail}")]
    InvalidManifest {
        /// Path of the loaded library.
        path: PathBuf,
        /// ABI status describing the manifest validation failure.
        status: FfiResult,
        /// Human-readable validation detail.
        detail: String,
    },

    /// Guest `BamManifest::abi_version` reported an incompatible packed version.
    #[error(
        "mod '{path}' reported incompatible ABI version 0x{guest_version:08x} (host ABI is 0x{host_version:08x})"
    )]
    IncompatibleAbiVersion {
        /// Path of the loaded library.
        path: PathBuf,
        /// Host's `ABI_VERSION`.
        host_version: u32,
        /// Guest's reported `BamManifest::abi_version`.
        guest_version: u32,
    },

    /// Required entry point (`bevy_abi_mod_init` or `bevy_abi_mod_shutdown`) was not exported.
    #[error("mod '{path}' is missing required symbol '{symbol}': {source}")]
    MissingEntryPoint {
        /// Path of the loaded library.
        path: PathBuf,
        /// Missing symbol name.
        symbol: &'static str,
        /// Underlying symbol lookup error.
        #[source]
        source: libloading::Error,
    },

    /// Guest `bevy_abi_mod_init` returned a non-OK [`FfiResult`].
    #[error("mod '{path}' initialization failed with {status}{}", if diagnostic.is_empty() { String::new() } else { format!(": {diagnostic}") })]
    InitFailed {
        /// Path of the loaded library.
        path: PathBuf,
        /// Error status returned by `bevy_abi_mod_init`.
        status: FfiResult,
        /// Optional host `last_error` diagnostic captured from `BamHostCtx`.
        diagnostic: String,
    },

    /// Guest `bevy_abi_mod_init` reported a caught guest panic (`FfiResult::PANIC_CAUGHT`).
    #[error("mod '{path}' panicked during initialization{}", if diagnostic.is_empty() { String::new() } else { format!(": {diagnostic}") })]
    InitPanicked {
        /// Path of the loaded library.
        path: PathBuf,
        /// Optional host `last_error` diagnostic captured from `BamHostCtx`.
        diagnostic: String,
    },

    /// Strict validation failed because a guest system queried unregistered components.
    #[error(
        "mod '{path}' system '{system_name}' references unregistered component(s): {}",
        missing.join(", ")
    )]
    MissingComponents {
        /// Path of the loaded library.
        path: PathBuf,
        /// Guest system name requesting unknown components.
        system_name: String,
        /// Unregistered component identifiers.
        missing: Vec<String>,
    },

    /// A guest system requested a custom [`ScheduleId`] (`>= 1024`) not registered in [`crate::ModScheduleMap`].
    #[error(
        "mod '{path}' system '{system_name}' targets unregistered custom schedule {schedule:?}"
    )]
    UnregisteredSchedule {
        /// Path of the loaded library.
        path: PathBuf,
        /// Guest system name.
        system_name: String,
        /// Unregistered schedule identifier.
        schedule: ScheduleId,
    },

    /// Strict ordering validation failed because a `before`/`after` target was not found.
    #[error(
        "mod '{path}' system '{system_name}' declares ordering dependency on unresolved target '{target}'"
    )]
    UnresolvedOrderingTarget {
        /// Path of the loaded library.
        path: PathBuf,
        /// Guest system name.
        system_name: String,
        /// Unresolved `before` or `after` target name.
        target: String,
    },

    /// Host attempted to register a custom schedule with a reserved built-in [`ScheduleId`] (`< 1024`).
    #[error("custom schedule ID {schedule:?} must be >= ScheduleId::CUSTOM_BASE (1024)")]
    InvalidScheduleRegistration {
        /// Invalid non-custom `ScheduleId`.
        schedule: ScheduleId,
    },

    /// Component or resource registration collided with an incompatible existing entry.
    #[error(transparent)]
    ComponentCollision(#[from] ComponentCollisionError),

    /// Attempted to unload a [`ModId`] that is not currently loaded in [`crate::ModHost`].
    #[error("mod {id:?} is not loaded")]
    ModNotFound {
        /// Unknown mod identifier.
        id: ModId,
    },

    /// Optional `BamManifest::save_state` callback returned a non-OK [`FfiResult`] during hot-reload.
    #[error("mod '{mod_name}' ({id:?}) save_state failed with {status}")]
    StateSaveFailed {
        /// Active mod identifier whose state save failed.
        id: ModId,
        /// Canonical mod name.
        mod_name: String,
        /// Status returned by `BamManifest::save_state`.
        status: FfiResult,
    },

    /// Optional `BamManifest::restore_state` callback returned a non-OK [`FfiResult`] during hot-reload.
    #[error("mod '{mod_name}' ({path}) restore_state failed with {status}")]
    StateRestoreFailed {
        /// Path of the reloaded shared library.
        path: PathBuf,
        /// Canonical mod name.
        mod_name: String,
        /// Status returned by `BamManifest::restore_state`.
        status: FfiResult,
    },

    /// Live archetype column schema migration failed during `reload_mod`.
    #[error("schema migration failed for component '{component_name}' ({status}): {detail}")]
    SchemaMigrationFailed {
        /// Canonical name of the dynamic component being migrated.
        component_name: String,
        /// Status returned by the guest witness symbol (`export_fields`, `init_default`, or `import_field`).
        status: FfiResult,
        /// Human-readable migration error detail.
        detail: String,
    },
}
