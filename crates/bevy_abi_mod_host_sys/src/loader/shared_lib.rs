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
    /// When `true` (default), inspects ELF/Mach-O headers and emits a structured `tracing::warn!`
    /// if the OS dynamic linker will pin the image resident across `dlclose` (`DF_1_NODELETE`,
    /// `DF_STATIC_TLS`, `PT_TLS`, or Mach-O TLS/TLV sections).
    pub warn_on_resident_image: bool,
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
            warn_on_resident_image: true,
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

    /// Enables or disables ELF/Mach-O dynamic linker residency warnings on load.
    #[inline]
    pub const fn with_warn_on_resident_image(mut self, enabled: bool) -> Self {
        self.warn_on_resident_image = enabled;
        self
    }
}

/// Binary container format detected when inspecting shared library residency indicators.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BinaryFormat {
    /// 32-bit ELF shared object.
    Elf32,
    /// 64-bit ELF shared object.
    Elf64,
    /// 32-bit Mach-O dynamic library.
    MachO32,
    /// 64-bit Mach-O dynamic library.
    MachO64,
    /// Universal / Fat Mach-O archive.
    MachOFat,
    /// Windows PE/COFF dynamic-link library.
    Pe,
    /// Unrecognized or truncated binary format.
    #[default]
    Unknown,
}

/// Dynamic linker residency indicators parsed safely from ELF/Mach-O/PE shared library headers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LibraryResidencyInfo {
    /// Detected binary container format.
    pub format: BinaryFormat,
    /// `true` if ELF `DT_FLAGS_1` sets `DF_1_NODELETE` (`0x0000_0008`), pinning the DSO across `dlclose`.
    pub nodelete: bool,
    /// `true` if ELF `DT_FLAGS` sets `DF_STATIC_TLS` (`0x0000_0010`).
    pub static_tls: bool,
    /// `true` if the binary contains TLS segments/sections (`PT_TLS`, Mach-O `MH_HAS_TLV_DESCRIPTORS`
    /// or `S_THREAD_LOCAL_*` / `__thread_vars` / `__thread_bss` / `__thread_data`).
    pub has_tls: bool,
    /// `true` if Mach-O header flags set `MH_NO_REEXPORTED_DYLIBS` (`0x0010_0000`).
    pub has_no_reexported_dylibs: bool,
}

impl LibraryResidencyInfo {
    /// Returns `true` if any indicator predicts the OS dynamic linker will pin this library
    /// resident across `dlclose` (making hot-unload unable to unmap the image).
    #[inline]
    pub const fn will_pin_resident(&self) -> bool {
        self.nodelete || self.static_tls || self.has_tls
    }
}

/// RAII file guard that unlinks a temporary shadow-copied shared library (and any shadow `.pdb`
/// sidecar) on `Drop`.
pub(crate) struct ShadowFileGuard {
    path: PathBuf,
    pdb_path: Option<PathBuf>,
}

impl ShadowFileGuard {
    #[inline]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[inline]
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn pdb_path(&self) -> Option<&Path> {
        self.pdb_path.as_deref()
    }
}

impl Drop for ShadowFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        if let Some(ref pdb) = self.pdb_path {
            let _ = std::fs::remove_file(pdb);
        }
    }
}

/// Wrapper pairing a [`libloading::Library`] with its optional [`ShadowFileGuard`] and parsed
/// OS dynamic-linker residency flags.
///
/// Field declaration order is load-bearing: Rust drops struct fields in declaration order, so
/// `library` (`dlclose` / `FreeLibrary`) runs **strictly before** `_shadow_guard` unlinks the
/// temporary file (and `.pdb` sidecar) on Windows, macOS, and Linux.
pub(crate) struct LoadedLibrary {
    pub(crate) library: Library,
    _shadow_guard: Option<ShadowFileGuard>,
    #[allow(dead_code)]
    pub(crate) residency: LibraryResidencyInfo,
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
            let residency = inspect_library_residency(&binary_bytes);
            let shadow_guard = stage_verified_shadow_bytes(
                source_path,
                options.shadow_dir.as_deref(),
                &binary_bytes,
            )?;
            let load_path = shadow_guard.path().to_path_buf();
            // SAFETY: `load_path` was exclusively created and written with the verified bytes.
            let lib = unsafe {
                Self::open_prepared_with_residency(
                    source_path,
                    load_path.clone(),
                    Some(shadow_guard),
                    residency,
                    options.warn_on_resident_image,
                )?
            };
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

        let residency = inspect_library_residency_file(&load_path).unwrap_or_default();

        // SAFETY: Caller guarantees `source_path` (and its verbatim shadow copy) is a valid shared library.
        let lib = unsafe {
            Self::open_prepared_with_residency(
                source_path,
                load_path.clone(),
                shadow_guard,
                residency,
                options.warn_on_resident_image,
            )?
        };
        Ok((lib, load_path))
    }

    /// # Safety
    /// `load_path` must be a valid shared library ready to map in-process.
    unsafe fn open_prepared_with_residency(
        source_path: &Path,
        load_path: PathBuf,
        shadow_guard: Option<ShadowFileGuard>,
        residency: LibraryResidencyInfo,
        warn_on_resident: bool,
    ) -> Result<Self, ModLoadError> {
        if warn_on_resident && residency.will_pin_resident() {
            tracing::warn!(
                path = %source_path.display(),
                format = ?residency.format,
                nodelete = residency.nodelete,
                static_tls = residency.static_tls,
                has_tls = residency.has_tls,
                has_no_reexported_dylibs = residency.has_no_reexported_dylibs,
                "guest shared library contains OS dynamic-linker residency flags/TLS sections and may remain mapped across dlclose"
            );
        }

        // SAFETY: If `Library::new` fails, `shadow_guard` drops immediately and unlinks the temp file.
        let library =
            unsafe { Library::new(&load_path) }.map_err(|source| ModLoadError::DlOpen {
                path: source_path.to_path_buf(),
                source,
            })?;

        Ok(Self {
            library,
            _shadow_guard: shadow_guard,
            residency,
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

    let pdb_path = stage_pdb_sidecar_and_patch_pe(source_path, &dest_path);

    Ok(ShadowFileGuard {
        path: dest_path,
        pdb_path,
    })
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
                let guard = ShadowFileGuard {
                    path: dest_path,
                    pdb_path: None,
                };
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

// ============================================================================
// Pure-safe ELF / Mach-O Residency Inspection & PE `.pdb` Sidecar Shadowing
// ============================================================================

#[inline]
fn read_u16_end(bytes: &[u8], off: usize, little_endian: bool) -> Option<u16> {
    let slice = bytes.get(off..off.checked_add(2)?)?;
    let arr: [u8; 2] = slice.try_into().ok()?;
    Some(if little_endian {
        u16::from_le_bytes(arr)
    } else {
        u16::from_be_bytes(arr)
    })
}

#[inline]
fn read_u32_end(bytes: &[u8], off: usize, little_endian: bool) -> Option<u32> {
    let slice = bytes.get(off..off.checked_add(4)?)?;
    let arr: [u8; 4] = slice.try_into().ok()?;
    Some(if little_endian {
        u32::from_le_bytes(arr)
    } else {
        u32::from_be_bytes(arr)
    })
}

#[inline]
fn read_u64_end(bytes: &[u8], off: usize, little_endian: bool) -> Option<u64> {
    let slice = bytes.get(off..off.checked_add(8)?)?;
    let arr: [u8; 8] = slice.try_into().ok()?;
    Some(if little_endian {
        u64::from_le_bytes(arr)
    } else {
        u64::from_be_bytes(arr)
    })
}

/// Inspects `path` (bounded to 64 MiB) for ELF/Mach-O dynamic-linker residency flags and TLS
/// sections.
pub fn inspect_library_residency_file(path: &Path) -> std::io::Result<LibraryResidencyInfo> {
    use std::io::Read;
    const MAX_INSPECT_BYTES: u64 = 64 * 1024 * 1024;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    file.take(MAX_INSPECT_BYTES).read_to_end(&mut buf)?;
    Ok(inspect_library_residency(&buf))
}

/// Pure safe parser inspecting ELF32/ELF64, Mach-O 32/64/Fat, and PE shared library bytes for
/// dynamic-linker residency indicators (`DF_1_NODELETE`, `DF_STATIC_TLS`, `PT_TLS`,
/// `MH_NO_REEXPORTED_DYLIBS`, `MH_HAS_TLV_DESCRIPTORS`, `__thread_vars`/`__thread_bss`).
pub fn inspect_library_residency(bytes: &[u8]) -> LibraryResidencyInfo {
    if bytes.starts_with(b"\x7fELF") {
        return inspect_elf_residency(bytes);
    }
    if bytes.len() >= 4 {
        let magic_be = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if magic_be == 0xcafe_babe || magic_be == 0xcafe_babf {
            return inspect_macho_fat_residency(bytes, magic_be == 0xcafe_babf);
        }
        let magic_le = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        match magic_le {
            0xfeed_face | 0xcefa_edfe | 0xfeed_facf | 0xcffa_edfe => {
                return inspect_macho_thin_residency(bytes);
            }
            _ => {}
        }
    }
    if is_valid_pe(bytes) {
        return LibraryResidencyInfo {
            format: BinaryFormat::Pe,
            ..Default::default()
        };
    }
    LibraryResidencyInfo::default()
}

fn inspect_elf_residency(bytes: &[u8]) -> LibraryResidencyInfo {
    const ELFCLASS32: u8 = 1;
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;
    const ELFDATA2MSB: u8 = 2;
    const PT_DYNAMIC: u32 = 2;
    const PT_TLS: u32 = 7;
    const DT_NULL: u64 = 0;
    const DT_FLAGS: u64 = 30;
    const DT_FLAGS_1: u64 = 0x6fff_fffb;
    const DF_STATIC_TLS: u64 = 0x0000_0010;
    const DF_1_NODELETE: u64 = 0x0000_0008;

    let Some(&ei_class) = bytes.get(4) else {
        return LibraryResidencyInfo::default();
    };
    let Some(&ei_data) = bytes.get(5) else {
        return LibraryResidencyInfo::default();
    };
    let le = match ei_data {
        ELFDATA2LSB => true,
        ELFDATA2MSB => false,
        _ => return LibraryResidencyInfo::default(),
    };

    let (format, is_64, phoff, phentsize, phnum) = match ei_class {
        ELFCLASS32 => {
            let Some(phoff) = read_u32_end(bytes, 28, le).map(u64::from) else {
                return LibraryResidencyInfo::default();
            };
            let Some(phentsize) = read_u16_end(bytes, 42, le).map(usize::from) else {
                return LibraryResidencyInfo::default();
            };
            let Some(phnum) = read_u16_end(bytes, 44, le).map(usize::from) else {
                return LibraryResidencyInfo::default();
            };
            (BinaryFormat::Elf32, false, phoff, phentsize, phnum)
        }
        ELFCLASS64 => {
            let Some(phoff) = read_u64_end(bytes, 32, le) else {
                return LibraryResidencyInfo::default();
            };
            let Some(phentsize) = read_u16_end(bytes, 54, le).map(usize::from) else {
                return LibraryResidencyInfo::default();
            };
            let Some(phnum) = read_u16_end(bytes, 56, le).map(usize::from) else {
                return LibraryResidencyInfo::default();
            };
            (BinaryFormat::Elf64, true, phoff, phentsize, phnum)
        }
        _ => return LibraryResidencyInfo::default(),
    };

    let mut info = LibraryResidencyInfo {
        format,
        ..Default::default()
    };
    let min_phdr = if is_64 { 56 } else { 32 };
    if phentsize < min_phdr {
        return info;
    }
    let Ok(phoff_usize) = usize::try_from(phoff) else {
        return info;
    };

    for idx in 0..phnum.min(4096) {
        let Some(entry_off) = idx
            .checked_mul(phentsize)
            .and_then(|d| phoff_usize.checked_add(d))
        else {
            break;
        };
        let Some(p_type) = read_u32_end(bytes, entry_off, le) else {
            break;
        };
        if p_type == PT_TLS {
            info.has_tls = true;
        } else if p_type == PT_DYNAMIC {
            let (dyn_off, dyn_size) = if is_64 {
                let Some(off) = read_u64_end(bytes, entry_off + 8, le) else {
                    continue;
                };
                let Some(sz) = read_u64_end(bytes, entry_off + 32, le) else {
                    continue;
                };
                (off, sz)
            } else {
                let Some(off) = read_u32_end(bytes, entry_off + 4, le).map(u64::from) else {
                    continue;
                };
                let Some(sz) = read_u32_end(bytes, entry_off + 16, le).map(u64::from) else {
                    continue;
                };
                (off, sz)
            };

            let (Ok(dyn_off), Ok(dyn_size)) = (usize::try_from(dyn_off), usize::try_from(dyn_size))
            else {
                continue;
            };
            let Some(dyn_end) = dyn_off.checked_add(dyn_size) else {
                continue;
            };
            let Some(dyn_slice) = bytes.get(dyn_off..dyn_end) else {
                continue;
            };
            let dyn_entry_size = if is_64 { 16 } else { 8 };
            for chunk in dyn_slice.chunks_exact(dyn_entry_size) {
                let (d_tag, d_val) = if is_64 {
                    (
                        read_u64_end(chunk, 0, le).unwrap_or(0),
                        read_u64_end(chunk, 8, le).unwrap_or(0),
                    )
                } else {
                    (
                        u64::from(read_u32_end(chunk, 0, le).unwrap_or(0)),
                        u64::from(read_u32_end(chunk, 4, le).unwrap_or(0)),
                    )
                };
                if d_tag == DT_NULL {
                    break;
                }
                if d_tag == DT_FLAGS && (d_val & DF_STATIC_TLS) != 0 {
                    info.static_tls = true;
                }
                if d_tag == DT_FLAGS_1 && (d_val & DF_1_NODELETE) != 0 {
                    info.nodelete = true;
                }
            }
        }
    }

    info
}

fn inspect_macho_fat_residency(bytes: &[u8], is_64: bool) -> LibraryResidencyInfo {
    let Some(nfat_arch) = read_u32_end(bytes, 4, false).map(|n| n as usize) else {
        return LibraryResidencyInfo::default();
    };
    if nfat_arch == 0 || nfat_arch > 32 {
        return LibraryResidencyInfo::default();
    }
    let arch_size = if is_64 { 32 } else { 20 };
    let mut info = LibraryResidencyInfo {
        format: BinaryFormat::MachOFat,
        ..Default::default()
    };
    let mut matched_slice = false;
    for idx in 0..nfat_arch {
        let Some(arch_off) = 8usize.checked_add(idx * arch_size) else {
            break;
        };
        let (slice_off, slice_size) = if is_64 {
            let Some(o) =
                read_u64_end(bytes, arch_off + 8, false).and_then(|v| usize::try_from(v).ok())
            else {
                continue;
            };
            let Some(s) =
                read_u64_end(bytes, arch_off + 16, false).and_then(|v| usize::try_from(v).ok())
            else {
                continue;
            };
            (o, s)
        } else {
            let Some(o) = read_u32_end(bytes, arch_off + 8, false).map(|v| v as usize) else {
                continue;
            };
            let Some(s) = read_u32_end(bytes, arch_off + 12, false).map(|v| v as usize) else {
                continue;
            };
            (o, s)
        };
        let Some(end) = slice_off.checked_add(slice_size) else {
            continue;
        };
        let Some(slice) = bytes.get(slice_off..end) else {
            continue;
        };
        let sub = inspect_macho_thin_residency(slice);
        if matches!(sub.format, BinaryFormat::MachO32 | BinaryFormat::MachO64) {
            matched_slice = true;
            info.nodelete |= sub.nodelete;
            info.static_tls |= sub.static_tls;
            info.has_tls |= sub.has_tls;
            info.has_no_reexported_dylibs |= sub.has_no_reexported_dylibs;
        }
    }
    if matched_slice {
        info
    } else {
        LibraryResidencyInfo::default()
    }
}

fn inspect_macho_thin_residency(bytes: &[u8]) -> LibraryResidencyInfo {
    const MH_MAGIC: u32 = 0xfeed_face;
    const MH_CIGAM: u32 = 0xcefa_edfe;
    const MH_MAGIC_64: u32 = 0xfeed_facf;
    const MH_CIGAM_64: u32 = 0xcffa_edfe;
    const MH_NO_REEXPORTED_DYLIBS: u32 = 0x0010_0000;
    const MH_HAS_TLV_DESCRIPTORS: u32 = 0x0080_0000;
    const LC_SEGMENT: u32 = 0x01;
    const LC_SEGMENT_64: u32 = 0x19;
    const SECTION_TYPE_MASK: u32 = 0x0000_00ff;
    const S_THREAD_LOCAL_REGULAR: u32 = 0x11;
    const S_THREAD_LOCAL_ZEROFILL: u32 = 0x12;
    const S_THREAD_LOCAL_VARIABLES: u32 = 0x13;
    const S_THREAD_LOCAL_VARIABLE_POINTERS: u32 = 0x14;
    const S_THREAD_LOCAL_INIT_FUNCTION_POINTERS: u32 = 0x15;

    let Some(raw_magic_le) = read_u32_end(bytes, 0, true) else {
        return LibraryResidencyInfo::default();
    };
    let (format, is_64, le) = match raw_magic_le {
        MH_MAGIC => (BinaryFormat::MachO32, false, true),
        MH_CIGAM => (BinaryFormat::MachO32, false, false),
        MH_MAGIC_64 => (BinaryFormat::MachO64, true, true),
        MH_CIGAM_64 => (BinaryFormat::MachO64, true, false),
        _ => return LibraryResidencyInfo::default(),
    };

    let hdr_size: usize = if is_64 { 32 } else { 28 };
    let Some(ncmds) = read_u32_end(bytes, 16, le).map(|v| v as usize) else {
        return LibraryResidencyInfo::default();
    };
    let Some(sizeofcmds) = read_u32_end(bytes, 20, le).map(|v| v as usize) else {
        return LibraryResidencyInfo::default();
    };
    let Some(flags) = read_u32_end(bytes, 24, le) else {
        return LibraryResidencyInfo::default();
    };

    let mut info = LibraryResidencyInfo {
        format,
        has_no_reexported_dylibs: (flags & MH_NO_REEXPORTED_DYLIBS) != 0,
        has_tls: (flags & MH_HAS_TLV_DESCRIPTORS) != 0,
        ..Default::default()
    };

    let cmds_end = hdr_size.saturating_add(sizeofcmds).min(bytes.len());
    let mut cursor = hdr_size;
    for _ in 0..ncmds.min(4096) {
        if cursor + 8 > cmds_end {
            break;
        }
        let Some(cmd) = read_u32_end(bytes, cursor, le) else {
            break;
        };
        let Some(cmdsize) = read_u32_end(bytes, cursor + 4, le).map(|v| v as usize) else {
            break;
        };
        if cmdsize < 8 || cursor.saturating_add(cmdsize) > cmds_end {
            break;
        }

        if (cmd == LC_SEGMENT && !is_64 && cmdsize >= 56)
            || (cmd == LC_SEGMENT_64 && is_64 && cmdsize >= 72)
        {
            let (seg_hdr_size, sect_size, nsects_off, sect_flags_rel) = if is_64 {
                (72usize, 80usize, 64usize, 64usize)
            } else {
                (56usize, 68usize, 48usize, 56usize)
            };
            let nsects = read_u32_end(bytes, cursor + nsects_off, le).unwrap_or(0) as usize;
            let mut sec_cursor = cursor + seg_hdr_size;
            for _ in 0..nsects.min(4096) {
                if sec_cursor + sect_size > cursor + cmdsize {
                    break;
                }
                let sect_name_bytes = &bytes[sec_cursor..sec_cursor + 16];
                let sect_name_len = sect_name_bytes
                    .iter()
                    .position(|&b| b == 0)
                    .unwrap_or(sect_name_bytes.len());
                let sect_name =
                    core::str::from_utf8(&sect_name_bytes[..sect_name_len]).unwrap_or("");
                let sect_flags = read_u32_end(bytes, sec_cursor + sect_flags_rel, le).unwrap_or(0);
                let sect_type = sect_flags & SECTION_TYPE_MASK;
                if matches!(
                    sect_type,
                    S_THREAD_LOCAL_REGULAR
                        | S_THREAD_LOCAL_ZEROFILL
                        | S_THREAD_LOCAL_VARIABLES
                        | S_THREAD_LOCAL_VARIABLE_POINTERS
                        | S_THREAD_LOCAL_INIT_FUNCTION_POINTERS
                ) || matches!(
                    sect_name,
                    "__thread_vars" | "__thread_bss" | "__thread_data"
                ) {
                    info.has_tls = true;
                }
                sec_cursor += sect_size;
            }
        }
        cursor += cmdsize;
    }

    info
}

fn pe_header_offset(bytes: &[u8]) -> Option<usize> {
    if !bytes.starts_with(b"MZ") {
        return None;
    }
    let lfanew = read_u32_end(bytes, 0x3c, true)? as usize;
    if bytes.get(lfanew..lfanew.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    Some(lfanew)
}

fn is_valid_pe(bytes: &[u8]) -> bool {
    pe_header_offset(bytes).is_some()
}

fn pe_rva_to_file_offset(
    bytes: &[u8],
    sections_off: usize,
    num_sections: usize,
    rva: u32,
) -> Option<usize> {
    for idx in 0..num_sections.min(256) {
        let off = sections_off.checked_add(idx * 40)?;
        let virt_size = read_u32_end(bytes, off + 8, true)?;
        let virt_addr = read_u32_end(bytes, off + 12, true)?;
        let raw_size = read_u32_end(bytes, off + 16, true)?;
        let raw_ptr = read_u32_end(bytes, off + 20, true)?;
        let span = virt_size.max(raw_size);
        if rva >= virt_addr && rva < virt_addr.saturating_add(span) {
            let delta = rva - virt_addr;
            if delta < raw_size {
                return Some(raw_ptr as usize + delta as usize);
            }
        }
    }
    None
}

/// Patches an embedded PE CodeView `RSDS` debug-directory `.pdb` path inside `pe_bytes` so
/// Windows debuggers/symbolizers open the shadow-copied `.pdb` instead of locking the build
/// directory's original `.pdb` (`LNK1201` prevention).
///
/// Returns `true` if at least one `RSDS` entry was updated.
pub fn patch_pe_codeview_pdb_path(pe_bytes: &mut [u8], shadow_pdb_path: &Path) -> bool {
    let Some(lfanew) = pe_header_offset(pe_bytes) else {
        return false;
    };
    let coff_off = lfanew + 4;
    let Some(num_sections) = read_u16_end(pe_bytes, coff_off + 2, true).map(usize::from) else {
        return false;
    };
    let Some(opt_size) = read_u16_end(pe_bytes, coff_off + 16, true).map(usize::from) else {
        return false;
    };
    let opt_off = coff_off + 20;
    if opt_size < 96 || opt_off + opt_size > pe_bytes.len() {
        return false;
    }
    let Some(opt_magic) = read_u16_end(pe_bytes, opt_off, true) else {
        return false;
    };
    let (num_rva_off, data_dir_off) = match opt_magic {
        0x10b => (opt_off + 92, opt_off + 96),
        0x20b => (opt_off + 108, opt_off + 112),
        _ => return false,
    };
    let Some(num_rva) = read_u32_end(pe_bytes, num_rva_off, true) else {
        return false;
    };
    // IMAGE_DIRECTORY_ENTRY_DEBUG == 6
    if num_rva <= 6 || data_dir_off + 7 * 8 > opt_off + opt_size {
        return false;
    }
    let Some(debug_rva) = read_u32_end(pe_bytes, data_dir_off + 6 * 8, true) else {
        return false;
    };
    let Some(debug_size) =
        read_u32_end(pe_bytes, data_dir_off + 6 * 8 + 4, true).map(|v| v as usize)
    else {
        return false;
    };
    if debug_rva == 0 || debug_size < 28 {
        return false;
    }

    let sections_off = opt_off + opt_size;
    let Some(debug_file_off) =
        pe_rva_to_file_offset(pe_bytes, sections_off, num_sections, debug_rva)
    else {
        return false;
    };
    let Some(debug_end) = debug_file_off.checked_add(debug_size) else {
        return false;
    };
    if debug_end > pe_bytes.len() {
        return false;
    }

    let full_pdb = shadow_pdb_path.to_string_lossy();
    let file_only = shadow_pdb_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&full_pdb);

    let mut patched = false;
    let entry_count = debug_size / 28;
    for i in 0..entry_count {
        let entry_off = debug_file_off + i * 28;
        let Some(dbg_type) = read_u32_end(pe_bytes, entry_off + 12, true) else {
            continue;
        };
        // IMAGE_DEBUG_TYPE_CODEVIEW == 2
        if dbg_type != 2 {
            continue;
        }
        let Some(cv_size) = read_u32_end(pe_bytes, entry_off + 16, true).map(|v| v as usize) else {
            continue;
        };
        let Some(cv_ptr) = read_u32_end(pe_bytes, entry_off + 24, true).map(|v| v as usize) else {
            continue;
        };
        if cv_size <= 25 || cv_ptr.saturating_add(cv_size) > pe_bytes.len() {
            continue;
        }
        if &pe_bytes[cv_ptr..cv_ptr + 4] != b"RSDS" {
            continue;
        }
        let path_slot = &mut pe_bytes[cv_ptr + 24..cv_ptr + cv_size];
        let replacement = if full_pdb.len() < path_slot.len() {
            full_pdb.as_bytes()
        } else if file_only.len() < path_slot.len() {
            file_only.as_bytes()
        } else {
            continue;
        };
        path_slot.fill(0);
        path_slot[..replacement.len()].copy_from_slice(replacement);
        patched = true;
    }

    if patched && opt_off + 68 <= pe_bytes.len() {
        // Clear PE OptionalHeader CheckSum so Windows loader does not reject modified DLL.
        pe_bytes[opt_off + 64..opt_off + 68].copy_from_slice(&0u32.to_le_bytes());
    }
    patched
}

fn stage_pdb_sidecar_and_patch_pe(source_path: &Path, dest_dll_path: &Path) -> Option<PathBuf> {
    let is_dll = source_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("dll"));
    if !is_dll {
        return None;
    }
    let source_pdb = source_path.with_extension("pdb");
    if !source_pdb.is_file() {
        return None;
    }
    let dest_pdb = dest_dll_path.with_extension("pdb");
    if std::fs::copy(&source_pdb, &dest_pdb).is_err() {
        return None;
    }
    if let Ok(mut dll_bytes) = std::fs::read(dest_dll_path)
        && patch_pe_codeview_pdb_path(&mut dll_bytes, &dest_pdb)
    {
        let _ = std::fs::write(dest_dll_path, &dll_bytes);
    }
    Some(dest_pdb)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_synthetic_elf64(has_pt_tls: bool, dt_flags: u64, dt_flags_1: u64) -> Vec<u8> {
        let phoff = 64usize;
        let phnum = if has_pt_tls { 2usize } else { 1usize };
        let dyn_off = phoff + phnum * 56;
        let dyn_entries = 3usize;
        let mut buf = vec![0u8; dyn_off + dyn_entries * 16];

        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 2; // ELFCLASS64
        buf[5] = 1; // ELFDATA2LSB
        buf[6] = 1; // EV_CURRENT
        buf[32..40].copy_from_slice(&(phoff as u64).to_le_bytes());
        buf[54..56].copy_from_slice(&56u16.to_le_bytes());
        buf[56..58].copy_from_slice(&(phnum as u16).to_le_bytes());

        // Phdr 0: PT_DYNAMIC
        let p0 = phoff;
        buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
        buf[p0 + 8..p0 + 16].copy_from_slice(&(dyn_off as u64).to_le_bytes());
        buf[p0 + 32..p0 + 40].copy_from_slice(&((dyn_entries * 16) as u64).to_le_bytes());

        if has_pt_tls {
            let p1 = phoff + 56;
            buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
        }

        // Dynamic entries: DT_FLAGS, DT_FLAGS_1, DT_NULL
        buf[dyn_off..dyn_off + 8].copy_from_slice(&30u64.to_le_bytes());
        buf[dyn_off + 8..dyn_off + 16].copy_from_slice(&dt_flags.to_le_bytes());
        buf[dyn_off + 16..dyn_off + 24].copy_from_slice(&0x6fff_fffbu64.to_le_bytes());
        buf[dyn_off + 24..dyn_off + 32].copy_from_slice(&dt_flags_1.to_le_bytes());
        buf
    }

    fn build_synthetic_elf32(has_pt_tls: bool, dt_flags: u32, dt_flags_1: u32) -> Vec<u8> {
        let phoff = 52usize;
        let phnum = if has_pt_tls { 2usize } else { 1usize };
        let dyn_off = phoff + phnum * 32;
        let mut buf = vec![0u8; dyn_off + 3 * 8];

        buf[0..4].copy_from_slice(b"\x7fELF");
        buf[4] = 1; // ELFCLASS32
        buf[5] = 1; // ELFDATA2LSB
        buf[28..32].copy_from_slice(&(phoff as u32).to_le_bytes());
        buf[42..44].copy_from_slice(&32u16.to_le_bytes());
        buf[44..46].copy_from_slice(&(phnum as u16).to_le_bytes());

        let p0 = phoff;
        buf[p0..p0 + 4].copy_from_slice(&2u32.to_le_bytes());
        buf[p0 + 4..p0 + 8].copy_from_slice(&(dyn_off as u32).to_le_bytes());
        buf[p0 + 16..p0 + 20].copy_from_slice(&24u32.to_le_bytes());

        if has_pt_tls {
            let p1 = phoff + 32;
            buf[p1..p1 + 4].copy_from_slice(&7u32.to_le_bytes());
        }

        buf[dyn_off..dyn_off + 4].copy_from_slice(&30u32.to_le_bytes());
        buf[dyn_off + 4..dyn_off + 8].copy_from_slice(&dt_flags.to_le_bytes());
        buf[dyn_off + 8..dyn_off + 12].copy_from_slice(&0x6fff_fffbu32.to_le_bytes());
        buf[dyn_off + 12..dyn_off + 16].copy_from_slice(&dt_flags_1.to_le_bytes());
        buf
    }

    fn build_synthetic_macho64(hdr_flags: u32, sect_name: &str, sect_flags: u32) -> Vec<u8> {
        let seg_cmd_size = 72usize + 80usize;
        let mut buf = vec![0u8; 32 + seg_cmd_size];

        buf[0..4].copy_from_slice(&0xfeed_facfu32.to_le_bytes());
        buf[16..20].copy_from_slice(&1u32.to_le_bytes()); // ncmds = 1
        buf[20..24].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
        buf[24..28].copy_from_slice(&hdr_flags.to_le_bytes());

        let lc = 32usize;
        buf[lc..lc + 4].copy_from_slice(&0x19u32.to_le_bytes()); // LC_SEGMENT_64
        buf[lc + 4..lc + 8].copy_from_slice(&(seg_cmd_size as u32).to_le_bytes());
        buf[lc + 64..lc + 68].copy_from_slice(&1u32.to_le_bytes()); // nsects = 1

        let sec = lc + 72;
        let name_bytes = sect_name.as_bytes();
        let copy_len = name_bytes.len().min(16);
        buf[sec..sec + copy_len].copy_from_slice(&name_bytes[..copy_len]);
        buf[sec + 64..sec + 68].copy_from_slice(&sect_flags.to_le_bytes());
        buf
    }

    fn build_synthetic_pe_with_rsds(original_pdb_slot: &str) -> Vec<u8> {
        let lfanew = 0x80usize;
        let opt_size = 240usize; // PE32+
        let sections_off = lfanew + 24 + opt_size;
        let rdata_raw_ptr = 0x400usize;
        let debug_dir_rva = 0x2000u32;
        let cv_rva = 0x2020u32;
        let cv_raw_ptr = rdata_raw_ptr + 0x20;
        let cv_size = 24 + original_pdb_slot.len() + 1;

        let mut buf = vec![0u8; cv_raw_ptr + cv_size + 32];
        buf[0..2].copy_from_slice(b"MZ");
        buf[0x3c..0x40].copy_from_slice(&(lfanew as u32).to_le_bytes());
        buf[lfanew..lfanew + 4].copy_from_slice(b"PE\0\0");

        let coff = lfanew + 4;
        buf[coff + 2..coff + 4].copy_from_slice(&1u16.to_le_bytes()); // 1 section
        buf[coff + 16..coff + 18].copy_from_slice(&(opt_size as u16).to_le_bytes());

        let opt = coff + 20;
        buf[opt..opt + 2].copy_from_slice(&0x20bu16.to_le_bytes()); // PE32+
        buf[opt + 64..opt + 68].copy_from_slice(&0x1234_5678u32.to_le_bytes()); // non-zero checksum
        buf[opt + 108..opt + 112].copy_from_slice(&16u32.to_le_bytes()); // NumberOfRvaAndSizes
        // IMAGE_DIRECTORY_ENTRY_DEBUG (index 6)
        buf[opt + 112 + 6 * 8..opt + 112 + 6 * 8 + 4].copy_from_slice(&debug_dir_rva.to_le_bytes());
        buf[opt + 112 + 6 * 8 + 4..opt + 112 + 6 * 8 + 8].copy_from_slice(&28u32.to_le_bytes());

        // Section 0 (.rdata): VA=0x2000, VirtSize=0x1000, RawSize=0x400, RawPtr=0x400
        buf[sections_off..sections_off + 6].copy_from_slice(b".rdata");
        buf[sections_off + 8..sections_off + 12].copy_from_slice(&0x1000u32.to_le_bytes());
        buf[sections_off + 12..sections_off + 16].copy_from_slice(&0x2000u32.to_le_bytes());
        buf[sections_off + 16..sections_off + 20].copy_from_slice(&0x400u32.to_le_bytes());
        buf[sections_off + 20..sections_off + 24]
            .copy_from_slice(&(rdata_raw_ptr as u32).to_le_bytes());

        // Debug directory entry at rdata_raw_ptr
        buf[rdata_raw_ptr + 12..rdata_raw_ptr + 16].copy_from_slice(&2u32.to_le_bytes()); // CODEVIEW
        buf[rdata_raw_ptr + 16..rdata_raw_ptr + 20]
            .copy_from_slice(&(cv_size as u32).to_le_bytes());
        buf[rdata_raw_ptr + 20..rdata_raw_ptr + 24].copy_from_slice(&cv_rva.to_le_bytes());
        buf[rdata_raw_ptr + 24..rdata_raw_ptr + 28]
            .copy_from_slice(&(cv_raw_ptr as u32).to_le_bytes());

        // RSDS CodeView record
        buf[cv_raw_ptr..cv_raw_ptr + 4].copy_from_slice(b"RSDS");
        buf[cv_raw_ptr + 24..cv_raw_ptr + 24 + original_pdb_slot.len()]
            .copy_from_slice(original_pdb_slot.as_bytes());

        buf
    }

    #[test]
    fn elf64_and_elf32_residency_detects_nodelete_static_tls_and_pt_tls() {
        let clean = build_synthetic_elf64(false, 0, 0);
        let clean_info = inspect_library_residency(&clean);
        assert_eq!(clean_info.format, BinaryFormat::Elf64);
        assert!(!clean_info.will_pin_resident());

        let pinned64 = build_synthetic_elf64(true, 0x10, 0x08);
        let info64 = inspect_library_residency(&pinned64);
        assert_eq!(info64.format, BinaryFormat::Elf64);
        assert!(info64.nodelete);
        assert!(info64.static_tls);
        assert!(info64.has_tls);
        assert!(info64.will_pin_resident());

        let pinned32 = build_synthetic_elf32(false, 0, 0x08);
        let info32 = inspect_library_residency(&pinned32);
        assert_eq!(info32.format, BinaryFormat::Elf32);
        assert!(info32.nodelete);
        assert!(!info32.has_tls);
        assert!(info32.will_pin_resident());
    }

    #[test]
    fn macho_residency_detects_thread_vars_tlv_and_reexport_flags() {
        let clean = build_synthetic_macho64(0x0010_0000, "__text", 0);
        let clean_info = inspect_library_residency(&clean);
        assert_eq!(clean_info.format, BinaryFormat::MachO64);
        assert!(clean_info.has_no_reexported_dylibs);
        assert!(!clean_info.has_tls);
        assert!(!clean_info.will_pin_resident());

        let tlv_sect = build_synthetic_macho64(0, "__thread_vars", 0x13);
        let tlv_info = inspect_library_residency(&tlv_sect);
        assert_eq!(tlv_info.format, BinaryFormat::MachO64);
        assert!(tlv_info.has_tls);
        assert!(tlv_info.will_pin_resident());

        let tlv_hdr = build_synthetic_macho64(0x0080_0000, "__data", 0);
        let hdr_info = inspect_library_residency(&tlv_hdr);
        assert!(hdr_info.has_tls);
        assert!(hdr_info.will_pin_resident());

        // Truncated/unknown data never panics.
        assert_eq!(
            inspect_library_residency(&[1, 2, 3]),
            LibraryResidencyInfo::default()
        );
    }

    #[test]
    fn shadow_copy_stages_and_cleans_up_pdb_sidecar_and_patches_rsds() {
        let dir = std::env::temp_dir().join(format!(
            "bam_pdb_shadow_test_{}_{}",
            std::process::id(),
            NEXT_SHADOW_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let src_dir = dir.join("src");
        let shd_dir = dir.join("shadow");
        std::fs::create_dir_all(&src_dir).unwrap();

        let dll_path = src_dir.join("sample_mod.dll");
        let pdb_path = src_dir.join("sample_mod.pdb");
        let orig_pdb_str = "C:\\very\\long\\build\\output\\target\\debug\\sample_mod.pdb";
        let pe_bytes = build_synthetic_pe_with_rsds(orig_pdb_str);

        std::fs::write(&dll_path, &pe_bytes).unwrap();
        std::fs::write(&pdb_path, b"FAKE_PDB_DATA").unwrap();

        let guard = stage_shadow_copy(&dll_path, Some(&shd_dir)).unwrap();
        let shadow_dll = guard.path().to_path_buf();
        let shadow_pdb = guard
            .pdb_path()
            .expect("expected shadow .pdb sidecar to be staged")
            .to_path_buf();

        assert!(shadow_dll.is_file());
        assert!(shadow_pdb.is_file());
        assert_eq!(
            shadow_pdb.file_stem(),
            shadow_dll.file_stem(),
            "shadow .pdb must share unique per-generation stem with shadow .dll"
        );
        assert_eq!(std::fs::read(&shadow_pdb).unwrap(), b"FAKE_PDB_DATA");

        let patched_dll = std::fs::read(&shadow_dll).unwrap();
        let expected_pdb_name = shadow_pdb.file_name().unwrap().to_str().unwrap();
        assert!(
            patched_dll
                .windows(expected_pdb_name.len())
                .any(|w| w == expected_pdb_name.as_bytes()),
            "RSDS CodeView PDB path should be rewritten to shadow .pdb"
        );
        // CheckSum cleared to 0 after patching.
        assert_eq!(&patched_dll[0x80 + 24 + 64..0x80 + 24 + 68], &[0, 0, 0, 0]);

        drop(guard);
        assert!(!shadow_dll.exists());
        assert!(!shadow_pdb.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
