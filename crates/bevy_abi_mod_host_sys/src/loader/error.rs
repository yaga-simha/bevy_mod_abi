//! Typed diagnostic errors for native shared-library mod loading and unloading.

use crate::loader::ModId;
use bevy_abi_mod_sys::FfiResult;
use core::fmt;
use std::path::PathBuf;

/// Checks SemVer compatibility between packed `(major << 24) | (minor << 16) | patch` ABI versions.
///
/// Delegates directly to [`bevy_abi_mod_sys::abi_version_is_compatible`].
#[inline]
pub const fn is_abi_version_compatible(host_packed: u32, guest_packed: u32) -> bool {
    bevy_abi_mod_sys::abi_version_is_compatible(host_packed, guest_packed)
}

/// Typed diagnostic error returned when loading or unloading a native mod fails.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModLoadError {
    /// Failed to open the shared library via OS dynamic linker (`dlopen` / `LoadLibrary`).
    DlOpen {
        /// Path passed to the loader.
        path: PathBuf,
        /// Underlying OS linker error message.
        message: String,
    },
    /// Required symbol (`bevy_abi_mod_init`) was not exported by the shared library.
    MissingEntryPoint {
        /// Path of the loaded library.
        path: PathBuf,
        /// Missing symbol name.
        symbol: &'static str,
        /// Underlying symbol lookup error message.
        message: String,
    },
    /// Guest `bevy_abi_mod_abi_version` reported an incompatible packed version.
    IncompatibleAbiVersion {
        /// Path of the loaded library.
        path: PathBuf,
        /// Host's `ABI_VERSION_PACKED`.
        host_version: u32,
        /// Guest's reported packed ABI version.
        guest_version: u32,
    },
    /// Guest `bevy_abi_mod_init` returned a non-OK [`FfiResult`].
    InitFailed {
        /// Path of the loaded library.
        path: PathBuf,
        /// Error status returned by `bevy_abi_mod_init`.
        status: FfiResult,
    },
    /// Guest `bevy_abi_mod_init` reported a caught panic (`FfiResult::PANIC_CAUGHT`).
    InitPanicked {
        /// Path of the loaded library.
        path: PathBuf,
    },
    /// Strict validation failed because a guest system queried unregistered components.
    MissingComponents {
        /// Path of the loaded library.
        path: PathBuf,
        /// Guest system name requesting unknown components.
        system_name: String,
        /// Unregistered component names.
        missing: Vec<String>,
    },
    /// Attempted to unload a [`ModId`] that is not currently loaded in [`crate::ModHost`].
    ModNotFound {
        /// Unknown mod identifier.
        id: ModId,
    },
}

impl ModLoadError {
    /// Maps this [`ModLoadError`] onto its C-ABI [`FfiResult`] status equivalent.
    #[inline]
    pub const fn to_ffi_result(&self) -> FfiResult {
        match self {
            Self::DlOpen { .. } | Self::MissingEntryPoint { .. } => FfiResult::INTERNAL_ERROR,
            Self::ModNotFound { .. } => FfiResult::NOT_FOUND,
            Self::IncompatibleAbiVersion { .. } => FfiResult::INVALID_VERSION,
            Self::InitFailed { status, .. } => *status,
            Self::InitPanicked { .. } => FfiResult::PANIC_CAUGHT,
            Self::MissingComponents { .. } => FfiResult::INVALID_COMPONENT,
        }
    }
}

impl From<ModLoadError> for FfiResult {
    #[inline]
    fn from(err: ModLoadError) -> Self {
        err.to_ffi_result()
    }
}

impl fmt::Display for ModLoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DlOpen { path, message } => {
                write!(f, "failed to load mod '{}': {}", path.display(), message)
            }
            Self::MissingEntryPoint {
                path,
                symbol,
                message,
            } => write!(
                f,
                "mod '{}' is missing required symbol '{}': {}",
                path.display(),
                symbol,
                message
            ),
            Self::IncompatibleAbiVersion {
                path,
                host_version,
                guest_version,
            } => write!(
                f,
                "mod '{}' reported incompatible ABI version 0x{:08x} (host expects compatible with 0x{:08x})",
                path.display(),
                guest_version,
                host_version
            ),
            Self::InitFailed { path, status } => write!(
                f,
                "mod '{}' initialization failed with {:?}",
                path.display(),
                status
            ),
            Self::InitPanicked { path } => {
                write!(f, "mod '{}' panicked during initialization", path.display())
            }
            Self::MissingComponents {
                path,
                system_name,
                missing,
            } => write!(
                f,
                "mod '{}' system '{}' references unregistered component(s): {}",
                path.display(),
                system_name,
                missing.join(", ")
            ),
            Self::ModNotFound { id } => write!(f, "mod {:?} is not loaded", id),
        }
    }
}

impl std::error::Error for ModLoadError {}
