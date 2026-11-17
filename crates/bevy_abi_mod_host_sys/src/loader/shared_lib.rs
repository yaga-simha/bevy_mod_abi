//! Shadow-copy hot-reload file management, `LoadOptions`, and per-OS-image `dlclose` ordering.

use crate::loader::error::ModLoadError;
use crate::system::ModErrorPolicy;
use bevy_abi_mod_sys::ScheduleId;
use bevy_platform::collections::HashMap;
use core::sync::atomic::{AtomicU64, Ordering};
use libloading::Library;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};

static NEXT_SHADOW_SEQ: AtomicU64 = AtomicU64::new(1);

/// Configuration options controlling how `ModWorldExt::load_mod_with` / `ModAppExt::load_mod_with`
/// opens, validates, and schedules a guest shared library.
#[derive(Clone, Debug)]
pub struct LoadOptions {
    /// When `true` (default in debug builds), copies the `.dll`/`.so`/`.dylib` to a unique
    /// temporary file before `dlopen` / `LoadLibraryW` and unlinks it automatically after `dlclose`.
    pub shadow_copy: bool,
    /// Optional directory in which shadow copies are staged (defaults to `std::env::temp_dir().join("bevy_abi_mod_shadows")`).
    pub shadow_dir: Option<PathBuf>,
    /// Optional [`ScheduleId`] overriding the schedule declared in every `BamSystemDesc` of this mod.
    pub schedule_override: Option<ScheduleId>,
    /// When `true`, fails load (rolling back cleanly) if any system references an unregistered
    /// component or declares an unresolved `before`/`after` ordering target.
    pub strict: bool,
    /// Optional per-mod [`ModErrorPolicy`] overriding the `World`'s default `ModErrorPolicyConfig`.
    pub error_policy: Option<ModErrorPolicy>,
}

impl Default for LoadOptions {
    #[inline]
    fn default() -> Self {
        Self {
            shadow_copy: cfg!(debug_assertions),
            shadow_dir: None,
            schedule_override: None,
            strict: false,
            error_policy: None,
        }
    }
}

impl LoadOptions {
    /// Creates default [`LoadOptions`].
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Enables or disables shadow-copy loading.
    #[inline]
    pub fn with_shadow_copy(mut self, enabled: bool) -> Self {
        self.shadow_copy = enabled;
        self
    }

    /// Configures a custom directory for shadow-copied shared libraries.
    #[inline]
    pub fn with_shadow_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.shadow_dir = Some(dir.into());
        self
    }

    /// Overrides the target [`ScheduleId`] for all systems registered by the mod.
    #[inline]
    pub const fn with_schedule_override(mut self, schedule: ScheduleId) -> Self {
        self.schedule_override = Some(schedule);
        self
    }

    /// Enables strict component and ordering validation (`strict = true`).
    #[inline]
    pub const fn strict(mut self) -> Self {
        self.strict = true;
        self
    }

    /// Overrides the [`ModErrorPolicy`] applied to systems registered by the mod.
    #[inline]
    pub const fn with_error_policy(mut self, policy: ModErrorPolicy) -> Self {
        self.error_policy = Some(policy);
        self
    }
}

/// RAII file guard that unlinks a temporary shadow-copied shared library on `Drop`.
pub(crate) struct ShadowFileGuard {
    path: PathBuf,
}

impl ShadowFileGuard {
    #[inline]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ShadowFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Wrapper pairing a [`libloading::Library`] with its optional [`ShadowFileGuard`].
///
/// Field declaration order is load-bearing: Rust drops struct fields in declaration order, so
/// `library` (`dlclose` / `FreeLibrary`) runs **strictly before** `_shadow_guard` unlinks the
/// temporary file on Windows, macOS, and Linux.
pub(crate) struct LoadedLibrary {
    pub(crate) library: Library,
    _shadow_guard: Option<ShadowFileGuard>,
}

impl LoadedLibrary {
    /// Stages a shadow copy if `options.shadow_copy` is enabled and opens the shared library.
    ///
    /// # Safety
    /// Caller must ensure `source_path` points to a trusted native mod shared library.
    pub(crate) unsafe fn open(
        source_path: &Path,
        options: &LoadOptions,
    ) -> Result<(Self, PathBuf), ModLoadError> {
        let shadow_guard = if options.shadow_copy {
            Some(stage_shadow_copy(
                source_path,
                options.shadow_dir.as_deref(),
            )?)
        } else {
            None
        };

        let load_path = shadow_guard
            .as_ref()
            .map(|g| g.path().to_path_buf())
            .unwrap_or_else(|| source_path.to_path_buf());

        // SAFETY: Caller guarantees `source_path` (and its verbatim shadow copy) is a valid shared library.
        // If `Library::new` fails, `shadow_guard` drops immediately and unlinks the temporary file.
        let library =
            unsafe { Library::new(&load_path) }.map_err(|source| ModLoadError::DlOpen {
                path: source_path.to_path_buf(),
                source,
            })?;

        Ok((
            Self {
                library,
                _shadow_guard: shadow_guard,
            },
            load_path,
        ))
    }
}

fn stage_shadow_copy(
    source_path: &Path,
    custom_dir: Option<&Path>,
) -> Result<ShadowFileGuard, ModLoadError> {
    if !source_path.exists() {
        let err = std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "source shared library '{}' does not exist",
                source_path.display()
            ),
        );
        return Err(ModLoadError::ShadowCopy {
            path: source_path.to_path_buf(),
            source: err,
        });
    }

    let dir = custom_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("bevy_abi_mod_shadows"));

    std::fs::create_dir_all(&dir).map_err(|source| ModLoadError::ShadowCopy {
        path: dir.clone(),
        source,
    })?;

    let stem = source_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("bam_mod");
    let ext = source_path.extension().and_then(|s| s.to_str());
    let pid = std::process::id();
    let seq = NEXT_SHADOW_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = match ext {
        Some(ext) if !ext.is_empty() => format!("{stem}.{pid}.{seq}.{ext}"),
        _ => format!("{stem}.{pid}.{seq}"),
    };
    let dest_path = dir.join(file_name);

    std::fs::copy(source_path, &dest_path).map_err(|source| ModLoadError::ShadowCopy {
        path: source_path.to_path_buf(),
        source,
    })?;

    Ok(ShadowFileGuard { path: dest_path })
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum SharedLibKey {
    #[cfg(unix)]
    UnixInode {
        dev: u64,
        ino: u64,
    },
    CanonicalPath(PathBuf),
}

impl SharedLibKey {
    fn from_path(path: &Path) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(path) {
                return Self::UnixInode {
                    dev: meta.dev(),
                    ino: meta.ino(),
                };
            }
        }
        Self::CanonicalPath(std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    }
}

/// Per-OS-image reference counter used solely to serialize `dlopen` / `dlclose` across worlds.
#[derive(Default)]
pub(crate) struct SharedLibState {
    pub(crate) active_count: usize,
}

static ACTIVE_SHARED_LIBS: LazyLock<Mutex<HashMap<SharedLibKey, Arc<Mutex<SharedLibState>>>>> =
    LazyLock::new(|| Mutex::new(HashMap::default()));

pub(crate) fn shared_lib_state_for_path(path: &Path) -> Arc<Mutex<SharedLibState>> {
    let key = SharedLibKey::from_path(path);
    let mut map = ACTIVE_SHARED_LIBS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(
        map.entry(key)
            .or_insert_with(|| Arc::new(Mutex::new(SharedLibState::default()))),
    )
}

/// Derives a deterministic fallback module name from a shared library filename
/// (`libfoo.dylib` / `libfoo.so` / `foo.dll` -> `"foo"`).
pub fn canonical_mod_name_from_path(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed_mod");
    stem.strip_prefix("lib")
        .filter(|rest| !rest.is_empty())
        .unwrap_or(stem)
        .to_string()
}
