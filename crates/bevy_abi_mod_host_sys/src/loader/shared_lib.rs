//! Shadow-copy hot-reload file management, `LoadOptions`, and per-OS-image `dlclose` ordering.

use crate::loader::error::ModLoadError;
#[cfg(feature = "signature-verification")]
use crate::loader::signing::{
    MAX_SIGNATURE_SIDECAR_BYTES, ModTrustPolicy, SignatureVerificationError, VerifiedModSignature,
    default_signature_path_for,
};
use crate::system::ModErrorPolicy;
use bevy_abi_mod_sys::ScheduleId;
use bevy_platform::collections::HashMap;
use core::sync::atomic::{AtomicU64, Ordering};
use libloading::Library;
#[cfg(feature = "signature-verification")]
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
#[cfg(feature = "signature-verification")]
use std::sync::OnceLock;
use std::sync::{Arc, LazyLock, Mutex};

static NEXT_SHADOW_SEQ: AtomicU64 = AtomicU64::new(1);
#[cfg(feature = "signature-verification")]
static VERIFIED_SHADOW_DIR: OnceLock<PathBuf> = OnceLock::new();

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
    /// Optional explicit path to the detached `.sig` sidecar used when `signature-verification`
    /// is enforced (defaults to `<mod_path>.sig`).
    pub signature_path: Option<PathBuf>,
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
            signature_path: None,
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

    /// Overrides the detached `.sig` sidecar path checked by `signature-verification`.
    #[inline]
    pub fn with_signature_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.signature_path = Some(path.into());
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
    /// Stages a shadow copy (or verifies & exclusively stages when `signature-verification`
    /// is active and enforced) and opens the shared library via `libloading`.
    ///
    /// # Safety
    /// When signature verification is disabled, caller must ensure `source_path` points to a
    /// trusted native mod shared library.
    #[cfg(feature = "signature-verification")]
    pub(crate) unsafe fn open(
        source_path: &Path,
        options: &LoadOptions,
        trust_policy: Option<&ModTrustPolicy>,
    ) -> Result<(Self, PathBuf, Option<VerifiedModSignature>), ModLoadError> {
        if let Some(policy) = trust_policy.filter(|p| p.is_enforced()) {
            let (binary_bytes, verified, _sig_path) =
                verify_source_and_sig(source_path, options, policy)?;
            let shadow_guard = stage_verified_shadow_bytes(
                source_path,
                options.shadow_dir.as_deref(),
                &binary_bytes,
            )?;
            let load_path = shadow_guard.path().to_path_buf();
            // SAFETY: `load_path` was exclusively created and written with the verified bytes.
            let lib =
                unsafe { Self::open_prepared(source_path, load_path.clone(), Some(shadow_guard))? };
            return Ok((lib, load_path, Some(verified)));
        }

        // SAFETY: Verification is disabled/unconfigured; caller guarantees `source_path` is trusted.
        let (lib, load_path) = unsafe { Self::open_unverified(source_path, options)? };
        Ok((lib, load_path, None))
    }

    /// Stages a shadow copy if `options.shadow_copy` is enabled and opens the shared library.
    ///
    /// # Safety
    /// Caller must ensure `source_path` points to a trusted native mod shared library.
    #[cfg(not(feature = "signature-verification"))]
    pub(crate) unsafe fn open(
        source_path: &Path,
        options: &LoadOptions,
    ) -> Result<(Self, PathBuf), ModLoadError> {
        // SAFETY: Forwarded directly to `open_unverified` under identical caller guarantees.
        unsafe { Self::open_unverified(source_path, options) }
    }

    /// # Safety
    /// Caller must ensure `source_path` points to a trusted native mod shared library.
    unsafe fn open_unverified(
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
        let lib = unsafe { Self::open_prepared(source_path, load_path.clone(), shadow_guard)? };
        Ok((lib, load_path))
    }

    /// # Safety
    /// `load_path` must be a valid shared library ready to map in-process.
    unsafe fn open_prepared(
        source_path: &Path,
        load_path: PathBuf,
        shadow_guard: Option<ShadowFileGuard>,
    ) -> Result<Self, ModLoadError> {
        // SAFETY: If `Library::new` fails, `shadow_guard` drops immediately and unlinks the temp file.
        let library =
            unsafe { Library::new(&load_path) }.map_err(|source| ModLoadError::DlOpen {
                path: source_path.to_path_buf(),
                source,
            })?;

        Ok(Self {
            library,
            _shadow_guard: shadow_guard,
        })
    }
}

fn ensure_shadow_dir(custom_dir: Option<&Path>) -> Result<PathBuf, ModLoadError> {
    let dir = custom_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std::env::temp_dir().join("bevy_abi_mod_shadows"));

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dir)
        .map_err(|source| ModLoadError::ShadowCopy {
            path: dir.clone(),
            source,
        })?;
    Ok(dir)
}

fn next_shadow_dest_path(source_path: &Path, dir: &Path) -> PathBuf {
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
    dir.join(file_name)
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

    let dir = ensure_shadow_dir(custom_dir)?;
    let dest_path = next_shadow_dest_path(source_path, &dir);

    std::fs::copy(source_path, &dest_path).map_err(|source| ModLoadError::ShadowCopy {
        path: source_path.to_path_buf(),
        source,
    })?;

    Ok(ShadowFileGuard { path: dest_path })
}

#[cfg(feature = "signature-verification")]
enum BoundedReadError {
    Io(std::io::Error),
    TooLarge(u64),
}

#[cfg(feature = "signature-verification")]
fn read_bounded_file(path: &Path, max_bytes: u64) -> Result<Vec<u8>, BoundedReadError> {
    let file = std::fs::File::open(path).map_err(BoundedReadError::Io)?;
    let meta = file.metadata().map_err(BoundedReadError::Io)?;
    if !meta.is_file() {
        return Err(BoundedReadError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("'{}' is not a regular file", path.display()),
        )));
    }
    let meta_len = meta.len();
    if meta_len > max_bytes {
        return Err(BoundedReadError::TooLarge(meta_len));
    }
    let cap = usize::try_from(meta_len).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)
        .map_err(BoundedReadError::Io)?;
    let actual_len = buf.len() as u64;
    if actual_len > max_bytes {
        return Err(BoundedReadError::TooLarge(actual_len));
    }
    Ok(buf)
}

#[cfg(feature = "signature-verification")]
fn verify_source_and_sig(
    source_path: &Path,
    options: &LoadOptions,
    policy: &ModTrustPolicy,
) -> Result<(Vec<u8>, VerifiedModSignature, PathBuf), ModLoadError> {
    let sig_path = options
        .signature_path
        .clone()
        .unwrap_or_else(|| default_signature_path_for(source_path));

    let max_bin = policy.max_binary_bytes();
    let binary_bytes = read_bounded_file(source_path, max_bin).map_err(|err| {
        let sv_err = match err {
            BoundedReadError::Io(source) => SignatureVerificationError::SourceBinaryRead { source },
            BoundedReadError::TooLarge(size) => SignatureVerificationError::BinaryTooLarge {
                size,
                limit: max_bin,
            },
        };
        ModLoadError::SignatureVerification {
            path: source_path.to_path_buf(),
            signature_path: sig_path.clone(),
            source: sv_err,
        }
    })?;

    let sig_bytes = read_bounded_file(&sig_path, MAX_SIGNATURE_SIDECAR_BYTES).map_err(|err| {
        let sv_err = match err {
            BoundedReadError::Io(source) if source.kind() == std::io::ErrorKind::NotFound => {
                SignatureVerificationError::MissingSignatureFile { source }
            }
            BoundedReadError::Io(source) => {
                SignatureVerificationError::SignatureFileRead { source }
            }
            BoundedReadError::TooLarge(size) => SignatureVerificationError::SignatureFileTooLarge {
                size,
                limit: MAX_SIGNATURE_SIDECAR_BYTES,
            },
        };
        ModLoadError::SignatureVerification {
            path: source_path.to_path_buf(),
            signature_path: sig_path.clone(),
            source: sv_err,
        }
    })?;

    let verified = policy
        .verify_bytes(&binary_bytes, &sig_bytes)
        .map_err(|source| ModLoadError::SignatureVerification {
            path: source_path.to_path_buf(),
            signature_path: sig_path.clone(),
            source,
        })?;

    Ok((binary_bytes, verified, sig_path))
}

#[cfg(feature = "signature-verification")]
fn validate_verified_shadow_dir_leaf(dir: &Path) -> Result<PathBuf, ModLoadError> {
    let meta = std::fs::symlink_metadata(dir).map_err(|source| ModLoadError::ShadowCopy {
        path: dir.to_path_buf(),
        source,
    })?;
    if meta.file_type().is_symlink() {
        return Err(ModLoadError::ShadowCopy {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "verified shadow directory must not be a symlink",
            ),
        });
    }
    if !meta.file_type().is_dir() {
        return Err(ModLoadError::ShadowCopy {
            path: dir.to_path_buf(),
            source: std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "verified shadow path is not a directory",
            ),
        });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o002) != 0 {
            return Err(ModLoadError::ShadowCopy {
                path: dir.to_path_buf(),
                source: std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "verified shadow directory has unsafe world-writable permissions ({mode:#o})"
                    ),
                ),
            });
        }
    }
    Ok(dir.to_path_buf())
}

#[cfg(feature = "signature-verification")]
fn ensure_verified_shadow_dir(custom_dir: Option<&Path>) -> Result<PathBuf, ModLoadError> {
    if let Some(dir) = custom_dir {
        match std::fs::symlink_metadata(dir) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut builder = std::fs::DirBuilder::new();
                builder.recursive(false);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::DirBuilderExt;
                    builder.mode(0o700);
                }
                match builder.create(dir) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(source) => {
                        return Err(ModLoadError::ShadowCopy {
                            path: dir.to_path_buf(),
                            source,
                        });
                    }
                }
            }
            Err(source) => {
                return Err(ModLoadError::ShadowCopy {
                    path: dir.to_path_buf(),
                    source,
                });
            }
        }
        return validate_verified_shadow_dir_leaf(dir);
    }

    if let Some(cached) = VERIFIED_SHADOW_DIR.get() {
        return validate_verified_shadow_dir_leaf(cached);
    }

    let base = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..16u64 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!("bevy_abi_mod_verified_{pid}_{nanos}_{attempt}"));
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        match builder.create(&candidate) {
            Ok(()) => {
                if VERIFIED_SHADOW_DIR.set(candidate.clone()).is_ok() {
                    return validate_verified_shadow_dir_leaf(&candidate);
                }
                let _ = std::fs::remove_dir(&candidate);
                if let Some(winner) = VERIFIED_SHADOW_DIR.get() {
                    return validate_verified_shadow_dir_leaf(winner);
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ModLoadError::ShadowCopy {
                    path: candidate,
                    source,
                });
            }
        }
    }

    Err(ModLoadError::ShadowCopy {
        path: base,
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "failed to allocate unique verified shadow directory after 16 attempts",
        ),
    })
}

#[cfg(feature = "signature-verification")]
fn stage_verified_shadow_bytes(
    source_path: &Path,
    custom_dir: Option<&Path>,
    bytes: &[u8],
) -> Result<ShadowFileGuard, ModLoadError> {
    let dir = ensure_verified_shadow_dir(custom_dir)?;
    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o700);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ: allow `LoadLibraryW` read/execute mapping while denying concurrent writers.
        open_opts.share_mode(0x0000_0001);
    }

    for attempt in 0..8u8 {
        let dest_path = next_shadow_dest_path(source_path, &dir);
        match open_opts.open(&dest_path) {
            Ok(mut file) => {
                let guard = ShadowFileGuard { path: dest_path };
                file.write_all(bytes)
                    .map_err(|source| ModLoadError::ShadowCopy {
                        path: source_path.to_path_buf(),
                        source,
                    })?;
                file.flush().map_err(|source| ModLoadError::ShadowCopy {
                    path: source_path.to_path_buf(),
                    source,
                })?;
                return Ok(guard);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists && attempt < 7 => continue,
            Err(source) => {
                return Err(ModLoadError::ShadowCopy {
                    path: source_path.to_path_buf(),
                    source,
                });
            }
        }
    }

    unreachable!("shadow staging loop returns by attempt 7");
}

/// Pre-verifies `source_path` and its detached `.sig` sidecar against `policy` without opening
/// `dlopen` / `LoadLibraryW` or creating a shadow copy.
///
/// Used by hot-reload (`reload_mod_with` / `reload_mod_from`) to reject unsigned or tampered
/// replacements before unloading the currently active mod.
#[cfg(feature = "signature-verification")]
pub fn preverify_mod_on_disk(
    source_path: &Path,
    options: &LoadOptions,
    policy: &ModTrustPolicy,
) -> Result<VerifiedModSignature, ModLoadError> {
    if !policy.is_enforced() {
        let sig_path = options
            .signature_path
            .clone()
            .unwrap_or_else(|| default_signature_path_for(source_path));
        return Err(ModLoadError::SignatureVerification {
            path: source_path.to_path_buf(),
            signature_path: sig_path,
            source: SignatureVerificationError::NoTrustAnchorsConfigured,
        });
    }
    let (_bytes, receipt, _sig_path) = verify_source_and_sig(source_path, options, policy)?;
    Ok(receipt)
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
