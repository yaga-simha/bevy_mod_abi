//! Pure-`std` recursive source/manifest watcher, incremental rebuilder, signer, and atomic stager
//! (`cargo bam watch`) for host `ModHotReloadPlugin` workflows.

use crate::builder;
use crate::scaffold::ModLanguage;
use crate::signing::{self, DEFAULT_NAMESPACE};
use crate::{CliError, ResolvedProject, atomic_write_bytes, resolve_project_info};
use bevy_abi_mod_trust::{PrivateKey, sign_mod_bytes_with_key};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

/// Default polling interval in milliseconds (`250 ms`).
pub const DEFAULT_POLL_MS: u64 = 250;

/// Default quiet-window debounce duration in milliseconds (`100 ms`).
pub const DEFAULT_DEBOUNCE_MS: u64 = 100;

/// Maximum individual source file size (`16 MiB`) hashed during fingerprinting.
const MAX_HASHED_SOURCE_BYTES: u64 = 16 * 1024 * 1024;

/// Directories always excluded from recursive source fingerprinting.
const IGNORED_DIRS: &[&str] = &[
    "target",
    "zig-out",
    ".zig-cache",
    "zig-cache",
    ".git",
    "node_modules",
];

/// Configuration for [`WatchRunner`] / [`run_watch`] (`cargo bam watch`).
#[derive(Clone, Debug)]
pub struct WatchConfig {
    /// Optional explicit path to `mod.toml` or `Cargo.toml`.
    pub manifest_path: Option<PathBuf>,
    /// Optional project directory containing `mod.toml` / `Cargo.toml`.
    pub dir: Option<PathBuf>,
    /// Build with release optimizations.
    pub release: bool,
    /// Optional staging root directory (`<out-dir>/<mod_id>/`) watched by `ModHotReloadPlugin`.
    pub out_dir: Option<PathBuf>,
    /// Optional OpenSSH Ed25519 private key to detach-sign every rebuilt library.
    pub sign_key: Option<PathBuf>,
    /// `SSHSIG` namespace used when [`Self::sign_key`] is set.
    pub namespace: String,
    /// Polling interval between filesystem snapshots.
    pub poll_interval: Duration,
    /// Quiet-window debounce duration before rebuilding after a detected change.
    pub debounce: Duration,
    /// Run only the initial build/sign/stage cycle and return immediately.
    pub once: bool,
    /// Optional upper bound on total completed build cycles (initial build is cycle `1`).
    pub max_cycles: Option<usize>,
}

impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            manifest_path: None,
            dir: None,
            release: false,
            out_dir: None,
            sign_key: None,
            namespace: DEFAULT_NAMESPACE.to_owned(),
            poll_interval: Duration::from_millis(DEFAULT_POLL_MS),
            debounce: Duration::from_millis(DEFAULT_DEBOUNCE_MS),
            once: false,
            max_cycles: None,
        }
    }
}

/// Deterministic fingerprint of a single watched source or manifest file (`mtime`, byte length,
/// and SHA-256 content digest).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileFingerprint {
    /// File length in bytes.
    pub len: u64,
    /// Modification timestamp seconds since `UNIX_EPOCH` (`0` if unavailable).
    pub modified_secs: u64,
    /// Modification timestamp sub-second nanoseconds (`0` if unavailable).
    pub modified_nanos: u32,
    /// SHA-256 content digest (`[0; 32]` only if file exceeds `MAX_HASHED_SOURCE_BYTES`).
    pub sha256: [u8; 32],
}

/// Ordered snapshot of all watched source and manifest files in a guest mod project.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WatchSnapshot {
    /// Sorted map of relative file paths to their fingerprints.
    pub files: BTreeMap<PathBuf, FileFingerprint>,
}

impl WatchSnapshot {
    /// Returns the sorted list of relative paths that were added, modified, or removed between
    /// `self` and `newer`.
    #[must_use]
    pub fn diff_paths(&self, newer: &Self) -> Vec<PathBuf> {
        let mut changed = Vec::new();
        for (path, fp) in &newer.files {
            if self.files.get(path) != Some(fp) {
                changed.push(path.clone());
            }
        }
        for path in self.files.keys() {
            if !newer.files.contains_key(path) {
                changed.push(path.clone());
            }
        }
        changed.sort();
        changed
    }
}

/// Outcome of a single completed build / sign / atomic-stage cycle in [`WatchRunner`].
#[derive(Clone, Debug)]
pub struct WatchCycleResult {
    /// 1-based build cycle number (`1` for initial build, `2+` for change-triggered rebuilds).
    pub cycle: usize,
    /// Detected guest language.
    pub language: ModLanguage,
    /// Canonical mod ID used for staging (`<out-dir>/<mod_id>/`).
    pub mod_id: String,
    /// Path to the built (or atomically staged) shared library.
    pub library_path: PathBuf,
    /// Atomically staged `mod.toml` path when `--out-dir` and `mod.toml` are present.
    pub staged_manifest_path: Option<PathBuf>,
    /// Atomically staged `.sig` sidecar path when `--key` is configured.
    pub signature_path: Option<PathBuf>,
    /// Relative paths that triggered this rebuild (empty on initial cycle `1`).
    pub changed_files: Vec<PathBuf>,
}

fn should_watch_file(language: ModLanguage, rel_path: &Path) -> bool {
    let Some(file_name) = rel_path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if file_name.starts_with('.') || file_name.ends_with('~') || file_name.ends_with(".swp") {
        return false;
    }
    if rel_path.components().count() == 1 && file_name == "mod.toml" {
        return true;
    }

    let ext = rel_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    let in_src = rel_path
        .components()
        .next()
        .and_then(|c| c.as_os_str().to_str())
        .is_some_and(|first| first == "src");

    match language {
        ModLanguage::Rust => {
            if rel_path.components().count() == 1 && matches!(file_name, "Cargo.toml" | "build.rs")
            {
                return true;
            }
            in_src && matches!(ext.as_deref(), Some("rs"))
        }
        ModLanguage::C99 => matches!(ext.as_deref(), Some("c" | "h")),
        ModLanguage::Cpp17 => matches!(
            ext.as_deref(),
            Some("cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "h" | "c")
        ),
        ModLanguage::Zig => {
            if rel_path.components().count() == 1
                && matches!(file_name, "build.zig" | "build.zig.zon")
            {
                return true;
            }
            matches!(ext.as_deref(), Some("zig" | "h" | "c"))
        }
    }
}

fn fingerprint_file(path: &Path, meta: &fs::Metadata) -> Result<FileFingerprint, CliError> {
    let len = meta.len();
    let (modified_secs, modified_nanos) = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map_or((0, 0), |d| (d.as_secs(), d.subsec_nanos()));

    let sha256 = if len <= MAX_HASHED_SOURCE_BYTES {
        let bytes = fs::read(path)?;
        Sha256::digest(&bytes).into()
    } else {
        [0u8; 32]
    };

    Ok(FileFingerprint {
        len,
        modified_secs,
        modified_nanos,
        sha256,
    })
}

fn is_excluded_out_dir(candidate: &Path, excluded_out_dir: Option<&Path>) -> bool {
    let Some(excluded) = excluded_out_dir else {
        return false;
    };
    if candidate == excluded {
        return true;
    }
    if let (Ok(canon_cand), Ok(canon_excl)) =
        (fs::canonicalize(candidate), fs::canonicalize(excluded))
    {
        return canon_cand == canon_excl;
    }
    false
}

fn collect_snapshot_recursive(
    root: &Path,
    current: &Path,
    language: ModLanguage,
    excluded_out_dir: Option<&Path>,
    out: &mut BTreeMap<PathBuf, FileFingerprint>,
) -> Result<(), CliError> {
    let mut entries: Vec<PathBuf> = Vec::new();
    for entry_res in fs::read_dir(current)? {
        let entry = entry_res?;
        entries.push(entry.path());
    }
    entries.sort();

    for path in entries {
        if is_excluded_out_dir(&path, excluded_out_dir) {
            continue;
        }
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        let Ok(rel) = path.strip_prefix(root) else {
            continue;
        };
        if meta.is_dir() {
            let dir_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if IGNORED_DIRS.contains(&dir_name) {
                continue;
            }
            collect_snapshot_recursive(root, &path, language, excluded_out_dir, out)?;
        } else if meta.is_file() && should_watch_file(language, rel) {
            let fp = fingerprint_file(&path, &meta)?;
            out.insert(rel.to_path_buf(), fp);
        }
    }
    Ok(())
}

/// Stateful, pure-`std` watcher and incremental builder for guest mods (`cargo bam watch`).
///
/// Designed for both CLI invocation ([`WatchRunner::run`]) and deterministic programmatic tests
/// ([`WatchRunner::run_initial_build`] + [`WatchRunner::poll_and_rebuild_if_changed`]).
#[derive(Debug)]
pub struct WatchRunner {
    config: WatchConfig,
    project: ResolvedProject,
    last_snapshot: Option<WatchSnapshot>,
    cycles_completed: usize,
}

impl WatchRunner {
    /// Creates a new [`WatchRunner`] after validating configuration, signing key availability,
    /// and guest mod project structure.
    pub fn new(config: WatchConfig) -> Result<Self, CliError> {
        if config.poll_interval.is_zero() {
            return Err(CliError::InvalidArgument(
                "`--poll-ms` must be at least 1 ms".to_owned(),
            ));
        }
        if config.max_cycles == Some(0) {
            return Err(CliError::InvalidArgument(
                "`--max-cycles` must be >= 1".to_owned(),
            ));
        }
        if config.sign_key.is_some() && config.namespace.is_empty() {
            return Err(CliError::InvalidArgument(
                "`--namespace` must not be empty when `--key` is provided".to_owned(),
            ));
        }
        if let Some(key_path) = &config.sign_key {
            PrivateKey::read_openssh_file(key_path).map_err(|e| {
                CliError::Crypto(format!(
                    "failed to read signing key {}: {e}",
                    key_path.display()
                ))
            })?;
        }

        let project = resolve_project_info(config.manifest_path.as_deref(), config.dir.as_deref())?;

        Ok(Self {
            config,
            project,
            last_snapshot: None,
            cycles_completed: 0,
        })
    }

    /// Returns the resolved guest mod project directory.
    #[must_use]
    pub fn project_dir(&self) -> &Path {
        &self.project.project_dir
    }

    /// Returns the detected guest language.
    #[must_use]
    pub const fn language(&self) -> ModLanguage {
        self.project.language
    }

    /// Returns the canonical mod ID used when staging into `--out-dir/<mod_id>/`.
    #[must_use]
    pub fn mod_id(&self) -> &str {
        &self.project.mod_id
    }

    /// Returns the number of completed build cycles so far.
    #[must_use]
    pub const fn cycles_completed(&self) -> usize {
        self.cycles_completed
    }

    /// Captures a deterministic [`WatchSnapshot`] of all watched source and manifest files.
    pub fn snapshot(&self) -> Result<WatchSnapshot, CliError> {
        let mut files = BTreeMap::new();
        collect_snapshot_recursive(
            &self.project.project_dir,
            &self.project.project_dir,
            self.project.language,
            self.config.out_dir.as_deref(),
            &mut files,
        )?;
        Ok(WatchSnapshot { files })
    }

    fn execute_build_and_stage(
        &mut self,
        changed_files: Vec<PathBuf>,
    ) -> Result<WatchCycleResult, CliError> {
        // Re-resolve manifest metadata on every cycle so edits to `mod.toml` are validated and picked up.
        self.project = resolve_project_info(
            self.config.manifest_path.as_deref(),
            self.config.dir.as_deref(),
        )?;

        let built = builder::build_mod(
            self.config.manifest_path.as_deref(),
            self.config.dir.as_deref(),
            self.config.release,
            None,
        )?;

        let lib_bytes = fs::read(&built.library_path)?;
        if lib_bytes.is_empty() {
            return Err(CliError::Command(format!(
                "built shared library {} is empty",
                built.library_path.display()
            )));
        }

        let sig_pem = if let Some(key_path) = &self.config.sign_key {
            let signing_key = PrivateKey::read_openssh_file(key_path).map_err(|e| {
                CliError::Crypto(format!(
                    "failed to read signing key {}: {e}",
                    key_path.display()
                ))
            })?;
            Some(
                sign_mod_bytes_with_key(&signing_key, &self.config.namespace, &lib_bytes)
                    .map_err(|e| CliError::Crypto(e.to_string()))?,
            )
        } else {
            None
        };

        let (final_lib_path, staged_manifest_path, signature_path) =
            if let Some(stage_root) = &self.config.out_dir {
                let stage_dir = stage_root.join(&self.project.mod_id);
                fs::create_dir_all(&stage_dir)?;

                // 1. Stage `mod.toml` atomically (only rewrite if missing or changed to avoid
                //    redundant `ModHotReloadPlugin` triggers when only code changed).
                let staged_manifest = if let Some(manifest_src) = &self.project.mod_toml_path {
                    let manifest_bytes = fs::read(manifest_src)?;
                    let dest_manifest = stage_dir.join("mod.toml");
                    let needs_write = fs::read(&dest_manifest)
                        .map(|existing| existing != manifest_bytes)
                        .unwrap_or(true);
                    if needs_write {
                        atomic_write_bytes(&dest_manifest, &manifest_bytes)?;
                    }
                    Some(dest_manifest)
                } else {
                    None
                };

                let file_name = built.library_path.file_name().ok_or_else(|| {
                    CliError::Command("built artifact has no filename".to_owned())
                })?;
                let staged_lib = stage_dir.join(file_name);

                // 2. Stage `.sig` atomically BEFORE the shared library so `ModHotReloadPlugin`
                //    sees the matching signature immediately when the new binary appears.
                let staged_sig = if let Some(pem) = sig_pem {
                    let sig_dest = signing::default_sig_path(&staged_lib);
                    atomic_write_bytes(&sig_dest, pem.as_bytes())?;
                    Some(sig_dest)
                } else {
                    None
                };

                // 3. Stage the shared library atomically LAST (temp file + rename).
                atomic_write_bytes(&staged_lib, &lib_bytes)?;

                (staged_lib, staged_manifest, staged_sig)
            } else {
                let sig_path = if let Some(pem) = sig_pem {
                    let sig_dest = signing::default_sig_path(&built.library_path);
                    atomic_write_bytes(&sig_dest, pem.as_bytes())?;
                    Some(sig_dest)
                } else {
                    None
                };
                (built.library_path, None, sig_path)
            };

        self.cycles_completed += 1;
        Ok(WatchCycleResult {
            cycle: self.cycles_completed,
            language: built.language,
            mod_id: self.project.mod_id.clone(),
            library_path: final_lib_path,
            staged_manifest_path,
            signature_path,
            changed_files,
        })
    }

    /// Runs the initial build, sign, and atomic stage cycle (`cycle == 1`), snapshotting sources
    /// prior to compilation so edits made mid-build are detected on the next poll.
    pub fn run_initial_build(&mut self) -> Result<WatchCycleResult, CliError> {
        let baseline = self.snapshot()?;
        let result = self.execute_build_and_stage(Vec::new())?;
        self.last_snapshot = Some(baseline);
        Ok(result)
    }

    /// Takes a fresh filesystem snapshot and triggers a rebuild/sign/atomic-stage cycle if any
    /// watched source or manifest file changed since the last build.
    pub fn poll_and_rebuild_if_changed(&mut self) -> Result<Option<WatchCycleResult>, CliError> {
        let Some(previous) = self.last_snapshot.clone() else {
            return self.run_initial_build().map(Some);
        };
        let current = self.snapshot()?;
        let changed = previous.diff_paths(&current);
        if changed.is_empty() {
            return Ok(None);
        }
        let result = self.execute_build_and_stage(changed)?;
        self.last_snapshot = Some(current);
        Ok(Some(result))
    }

    /// Runs the debounced watch loop until `--once` / `--max-cycles` is satisfied (or forever if
    /// neither bound is configured).
    pub fn run(&mut self) -> Result<Vec<WatchCycleResult>, CliError> {
        let target_cycles = if self.config.once {
            Some(1)
        } else {
            self.config.max_cycles
        };
        let mut results = Vec::new();

        match self.run_initial_build() {
            Ok(first) => {
                print_cycle_summary(&first);
                results.push(first);
            }
            Err(err) => {
                if target_cycles.is_some() {
                    return Err(err);
                }
                // Unbounded interactive watch: record baseline and wait for the user to fix errors.
                self.last_snapshot = Some(self.snapshot()?);
                eprintln!("cargo-bam watch: initial build failed (waiting for edits): {err}");
            }
        }

        if target_cycles.is_some_and(|max| self.cycles_completed >= max) {
            return Ok(results);
        }

        let mut pending: Option<(WatchSnapshot, Instant)> = None;
        loop {
            thread::sleep(self.config.poll_interval);
            let current = self.snapshot()?;

            let unchanged = self
                .last_snapshot
                .as_ref()
                .is_some_and(|prev| prev == &current);
            if unchanged {
                pending = None;
                continue;
            }

            let settled = match &pending {
                Some((prev_pending, since)) if prev_pending == &current => {
                    since.elapsed() >= self.config.debounce
                }
                _ => {
                    pending = Some((current.clone(), Instant::now()));
                    self.config.debounce.is_zero()
                }
            };

            if !settled {
                continue;
            }

            pending = None;
            let changed = self
                .last_snapshot
                .as_ref()
                .map_or_else(Vec::new, |prev| prev.diff_paths(&current));

            match self.execute_build_and_stage(changed) {
                Ok(cycle_res) => {
                    self.last_snapshot = Some(current);
                    print_cycle_summary(&cycle_res);
                    results.push(cycle_res);
                    if target_cycles.is_some_and(|max| self.cycles_completed >= max) {
                        return Ok(results);
                    }
                }
                Err(err) => {
                    // Advance snapshot so we do not spin-rebuild identical broken sources.
                    self.last_snapshot = Some(current);
                    eprintln!("cargo-bam watch: rebuild failed (waiting for edits): {err}");
                }
            }
        }
    }
}

fn print_cycle_summary(res: &WatchCycleResult) {
    if res.changed_files.is_empty() {
        println!(
            "[watch cycle {}] built {} mod '{}' -> {}",
            res.cycle,
            res.language,
            res.mod_id,
            res.library_path.display()
        );
    } else {
        let files: Vec<String> = res
            .changed_files
            .iter()
            .map(|p| p.display().to_string())
            .collect();
        println!(
            "[watch cycle {}] rebuilt {} mod '{}' (changed: {}) -> {}",
            res.cycle,
            res.language,
            res.mod_id,
            files.join(", "),
            res.library_path.display()
        );
    }
    if let Some(sig) = &res.signature_path {
        println!("  signature: {}", sig.display());
    }
}

/// Convenience entrypoint creating a [`WatchRunner`] and executing [`WatchRunner::run`].
pub fn run_watch(config: WatchConfig) -> Result<Vec<WatchCycleResult>, CliError> {
    let mut runner = WatchRunner::new(config)?;
    runner.run()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn should_watch_file_filters_by_language_and_ignores_temp_files() {
        assert!(should_watch_file(ModLanguage::Rust, Path::new("mod.toml")));
        assert!(should_watch_file(
            ModLanguage::Rust,
            Path::new("Cargo.toml")
        ));
        assert!(should_watch_file(
            ModLanguage::Rust,
            Path::new("src/lib.rs")
        ));
        assert!(should_watch_file(
            ModLanguage::Rust,
            Path::new("src/nested/mod.rs")
        ));
        assert!(!should_watch_file(
            ModLanguage::Rust,
            Path::new("Cargo.lock")
        ));
        assert!(!should_watch_file(
            ModLanguage::Rust,
            Path::new("src/.lib.rs.swp")
        ));

        assert!(should_watch_file(ModLanguage::C99, Path::new("src/mod.c")));
        assert!(should_watch_file(
            ModLanguage::C99,
            Path::new("include/bevy_abi_mod.h")
        ));
        assert!(!should_watch_file(
            ModLanguage::C99,
            Path::new("target/debug/libfoo.so")
        ));

        assert!(should_watch_file(
            ModLanguage::Cpp17,
            Path::new("src/mod.cpp")
        ));
        assert!(should_watch_file(
            ModLanguage::Cpp17,
            Path::new("include/bevy_abi_mod.hpp")
        ));

        assert!(should_watch_file(ModLanguage::Zig, Path::new("build.zig")));
        assert!(should_watch_file(
            ModLanguage::Zig,
            Path::new("src/root.zig")
        ));
        assert!(should_watch_file(ModLanguage::Zig, Path::new("mod.toml")));
    }

    #[test]
    fn snapshot_detects_same_length_content_change_and_ignores_target_dir() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir_all(tmp.path().join("src")).unwrap();
        fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        fs::write(
            tmp.path().join("mod.toml"),
            "[mod]\nid = \"snap_c\"\nversion = \"0.1.0\"\nabi_version = \"^0.3.0\"\nlanguage = \"c99\"\n",
        )
        .unwrap();
        fs::write(tmp.path().join("src/mod.c"), "int val = 1;\n").unwrap();
        fs::write(tmp.path().join("target/debug/ignored.c"), "int x = 0;\n").unwrap();

        let runner = WatchRunner::new(WatchConfig {
            dir: Some(tmp.path().to_path_buf()),
            ..WatchConfig::default()
        })
        .unwrap();

        let snap1 = runner.snapshot().unwrap();
        assert!(snap1.files.contains_key(Path::new("mod.toml")));
        assert!(snap1.files.contains_key(Path::new("src/mod.c")));
        assert!(
            !snap1
                .files
                .contains_key(Path::new("target/debug/ignored.c"))
        );

        // Same-length, immediate edit must still produce a distinct fingerprint via SHA-256!
        fs::write(tmp.path().join("src/mod.c"), "int val = 2;\n").unwrap();
        let snap2 = runner.snapshot().unwrap();
        let diff = snap1.diff_paths(&snap2);
        assert_eq!(diff, vec![PathBuf::from("src/mod.c")]);
    }
}
