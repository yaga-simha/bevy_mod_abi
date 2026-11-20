#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Library implementation for the `cargo-bam` modding CLI toolchain (`ABI 0.3.0`).

pub mod builder;
pub mod dag;
pub mod doctor;
pub mod pack;
pub mod scaffold;
pub mod sdk;
pub mod signing;
pub mod watch;

use bevy_abi_mod_manifest::{validate_manifest_entry_path, validate_mod_id};
use clap::{Parser, Subcommand};
use dag::ValidatedModManifest;
use scaffold::ModLanguage;
use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use thiserror::Error;

/// Typed error returned by `cargo-bam` CLI operations.
#[derive(Debug, Error)]
pub enum CliError {
    /// Invalid CLI argument or identifier.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// Target directory already exists.
    #[error("target directory already exists: {}", .0.display())]
    AlreadyExists(PathBuf),
    /// Invalid or missing `mod.toml` / `Cargo.toml`.
    #[error("invalid manifest: {0}")]
    InvalidManifest(String),
    /// External compiler/build command failed.
    #[error("build command failed: {0}")]
    Command(String),
    /// Cryptographic keygen/signing/verification failure.
    #[error("crypto error: {0}")]
    Crypto(String),
    /// Underlying filesystem I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedProject {
    pub(crate) project_dir: PathBuf,
    pub(crate) mod_toml_path: Option<PathBuf>,
    pub(crate) manifest: Option<ValidatedModManifest>,
    pub(crate) language: ModLanguage,
    pub(crate) mod_id: String,
    pub(crate) lib_stem: String,
}

pub(crate) fn dylib_naming() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

/// Reads a regular file from disk bounded by `max_bytes`, failing cleanly on directories/FIFOs
/// or oversized files.
pub(crate) fn read_file_bounded(
    path: &Path,
    max_bytes: u64,
    label: &str,
) -> Result<Vec<u8>, CliError> {
    let file = fs::File::open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(CliError::InvalidArgument(format!(
            "{label} '{}' is not a regular file",
            path.display()
        )));
    }
    if meta.len() > max_bytes {
        return Err(CliError::InvalidArgument(format!(
            "{label} '{}' ({} bytes) exceeds limit ({max_bytes} bytes)",
            path.display(),
            meta.len()
        )));
    }
    let cap = usize::try_from(meta.len()).unwrap_or(0);
    let mut buf = Vec::with_capacity(cap);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut buf)?;
    if buf.len() as u64 > max_bytes {
        return Err(CliError::InvalidArgument(format!(
            "{label} '{}' exceeded limit ({max_bytes} bytes) while reading",
            path.display()
        )));
    }
    Ok(buf)
}

/// Validates `mod_id` and safely creates/verifies `<stage_root>/<mod_id>` without allowing
/// traversal, unsafe identifiers, symlink escapes, or writes outside `stage_root`.
pub(crate) fn validated_stage_dir(stage_root: &Path, mod_id: &str) -> Result<PathBuf, CliError> {
    validate_mod_id(mod_id).map_err(|reason| {
        CliError::InvalidManifest(format!(
            "refusing to stage unsafe mod identifier '{mod_id}': {reason}"
        ))
    })?;

    if let Ok(meta) = fs::symlink_metadata(stage_root)
        && meta.file_type().is_symlink()
    {
        return Err(CliError::InvalidArgument(format!(
            "staging root must not be a symlink: {}",
            stage_root.display()
        )));
    }
    fs::create_dir_all(stage_root)?;
    let canon_root = fs::canonicalize(stage_root)?;

    let stage_dir = stage_root.join(mod_id);
    if let Ok(meta) = fs::symlink_metadata(&stage_dir)
        && meta.file_type().is_symlink()
    {
        return Err(CliError::InvalidArgument(format!(
            "staging directory must not be a symlink: {}",
            stage_dir.display()
        )));
    }
    fs::create_dir_all(&stage_dir)?;
    let canon_stage = fs::canonicalize(&stage_dir)?;
    if !canon_stage.starts_with(&canon_root) || canon_stage == canon_root {
        return Err(CliError::InvalidManifest(format!(
            "staging directory '{}' escapes staging root '{}'",
            canon_stage.display(),
            canon_root.display()
        )));
    }
    Ok(stage_dir)
}

#[derive(Deserialize)]
struct CargoTomlPartial {
    package: Option<CargoPackagePartial>,
    lib: Option<CargoLibPartial>,
}

#[derive(Deserialize)]
struct CargoPackagePartial {
    name: String,
}

#[derive(Deserialize)]
struct CargoLibPartial {
    name: Option<String>,
}

pub(crate) fn detect_rust_crate_stem(cargo_toml: &Path) -> Result<String, CliError> {
    let raw = read_file_bounded(
        cargo_toml,
        bevy_abi_mod_manifest::MAX_MANIFEST_BYTES,
        "Cargo.toml",
    )?;
    let content = String::from_utf8(raw).map_err(|_| {
        CliError::InvalidManifest(format!("{} is not valid UTF-8", cargo_toml.display()))
    })?;
    let parsed: CargoTomlPartial = toml::from_str(&content)
        .map_err(|e| CliError::InvalidManifest(format!("{}: {e}", cargo_toml.display())))?;
    let name = parsed
        .lib
        .and_then(|l| l.name)
        .or_else(|| parsed.package.map(|p| p.name))
        .ok_or_else(|| {
            CliError::InvalidManifest(format!(
                "{} is missing `[package].name`",
                cargo_toml.display()
            ))
        })?;
    let stem = name.replace('-', "_");
    validate_mod_id(&stem).map_err(|reason| {
        CliError::InvalidManifest(format!(
            "{} has unsafe crate name '{name}': {reason}",
            cargo_toml.display()
        ))
    })?;
    Ok(stem)
}

fn parse_language_str(raw: &str) -> Option<ModLanguage> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Some(ModLanguage::Rust),
        "c" | "c99" => Some(ModLanguage::C99),
        "cpp" | "c++" | "cpp17" | "c++17" => Some(ModLanguage::Cpp17),
        "zig" => Some(ModLanguage::Zig),
        _ => None,
    }
}

pub(crate) fn resolve_project_info(
    manifest_path: Option<&Path>,
    dir: Option<&Path>,
) -> Result<ResolvedProject, CliError> {
    let (project_dir, mod_toml_path) = if let Some(mp) = manifest_path {
        let file_name = mp.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if file_name == "mod.toml" {
            let pdir = mp
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            (pdir, Some(mp.to_path_buf()))
        } else if file_name == "Cargo.toml" {
            let pdir = mp
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            let candidate = pdir.join("mod.toml");
            (pdir, candidate.is_file().then_some(candidate))
        } else {
            return Err(CliError::InvalidArgument(format!(
                "--manifest-path must point to `mod.toml` or `Cargo.toml`, got {}",
                mp.display()
            )));
        }
    } else {
        let pdir = dir.unwrap_or_else(|| Path::new(".")).to_path_buf();
        let candidate = pdir.join("mod.toml");
        (pdir, candidate.is_file().then_some(candidate))
    };

    let manifest = if let Some(ref mp) = mod_toml_path {
        Some(ValidatedModManifest::from_file(mp)?)
    } else {
        None
    };

    let cargo_toml = project_dir.join("Cargo.toml");
    let explicit_lang = manifest
        .as_ref()
        .and_then(|m| m.language.as_deref())
        .and_then(parse_language_str);

    let language = if let Some(lang) = explicit_lang {
        lang
    } else if cargo_toml.is_file() {
        ModLanguage::Rust
    } else if project_dir.join("build.zig").is_file() {
        ModLanguage::Zig
    } else if project_dir.join("src/mod.cpp").is_file() || project_dir.join("mod.cpp").is_file() {
        ModLanguage::Cpp17
    } else if project_dir.join("src/mod.c").is_file() || project_dir.join("mod.c").is_file() {
        ModLanguage::C99
    } else {
        return Err(CliError::InvalidManifest(format!(
            "could not determine mod language in {} (missing `mod.toml`, `Cargo.toml`, `build.zig`, `src/mod.c`, or `src/mod.cpp`)",
            project_dir.display()
        )));
    };

    let mod_id = match manifest.as_ref() {
        Some(m) => {
            validate_mod_id(&m.id).map_err(|reason| {
                CliError::InvalidManifest(format!("invalid [mod].id '{}': {reason}", m.id))
            })?;
            if let Some(ref entry) = m.entry {
                validate_manifest_entry_path(entry).map_err(|reason| {
                    CliError::InvalidManifest(format!("invalid [mod].entry '{entry}': {reason}"))
                })?;
            }
            m.id.clone()
        }
        None => {
            let derived = project_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("bam_mod")
                .to_owned();
            validate_mod_id(&derived).map_err(|reason| {
                CliError::InvalidManifest(format!(
                    "derived mod identifier '{derived}' from '{}' is unsafe: {reason} (add an explicit [mod].id in mod.toml)",
                    project_dir.display()
                ))
            })?;
            derived
        }
    };

    let lib_stem = if language == ModLanguage::Rust && cargo_toml.is_file() {
        detect_rust_crate_stem(&cargo_toml)?
    } else {
        let stem = manifest
            .as_ref()
            .and_then(|m| m.entry.clone())
            .map(|e| {
                Path::new(&e)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&mod_id)
                    .trim_start_matches("lib")
                    .to_owned()
            })
            .unwrap_or_else(|| mod_id.clone())
            .replace('-', "_");
        validate_mod_id(&stem).map_err(|reason| {
            CliError::InvalidManifest(format!("derived library stem '{stem}' is unsafe: {reason}"))
        })?;
        stem
    };

    Ok(ResolvedProject {
        project_dir,
        mod_toml_path,
        manifest,
        language,
        mod_id,
        lib_stem,
    })
}

pub(crate) fn atomic_write_bytes(dest: &Path, bytes: &[u8]) -> Result<(), CliError> {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    let parent = dest
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;

    let base_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("artifact");
    let pid = std::process::id();

    let (mut file, tmp_path) = loop {
        let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(".{base_name}.bamtmp.{pid}.{seq}"));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(f) => break (f, candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(CliError::Io(err)),
        }
    };

    let write_res = file
        .write_all(bytes)
        .and_then(|()| file.flush())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(err) = write_res {
        let _ = fs::remove_file(&tmp_path);
        return Err(CliError::Io(err));
    }

    if let Err(err) = fs::rename(&tmp_path, dest) {
        let _ = fs::remove_file(&tmp_path);
        return Err(CliError::Io(err));
    }
    Ok(())
}

#[derive(Debug, Parser)]
#[command(
    name = "cargo-bam",
    bin_name = "cargo bam",
    version,
    about = "Build, scaffold, package, watch, export SDK headers, and sign bevy_abi_mod guest mods"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Scaffold a new buildable guest mod project (Rust, C99, C++17, or Zig).
    New {
        /// Mod crate/library name.
        name: String,
        /// Target guest implementation language.
        #[arg(long, value_enum, default_value_t = ModLanguage::Rust)]
        lang: ModLanguage,
        /// Parent directory in which `<name>` will be created.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Optional path to `crates/bevy_abi_mod_guest` for Rust mods.
        #[arg(long)]
        guest_sdk_path: Option<PathBuf>,
    },
    /// Export `bevy_abi_mod.h`, `bevy_abi_mod.hpp`, and `bevy_abi_mod.zig` SDK headers.
    #[command(visible_alias = "header")]
    ExportSdk {
        /// Output directory for exported headers.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a guest mod shared library and optionally stage it alongside `mod.toml` (and `.sig`).
    Build {
        /// Explicit path to `mod.toml` or `Cargo.toml`.
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Project directory containing `mod.toml` / `Cargo.toml`.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Build with release optimizations.
        #[arg(long)]
        release: bool,
        /// Optional staging root directory (`<out-dir>/<mod_id>/` with library, `mod.toml`, and optional `.sig`).
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Optional OpenSSH Ed25519 private key to detach-sign the built shared library.
        #[arg(long)]
        sign_key: Option<PathBuf>,
        /// `SSHSIG` namespace used when `--sign-key` is passed.
        #[arg(long, default_value = signing::DEFAULT_NAMESPACE)]
        sign_namespace: String,
    },
    /// Package a guest mod (`mod.toml`, shared library, and optional `assets/`) into a deterministic `.bampack` archive.
    Pack {
        /// Project directory containing `mod.toml` / `Cargo.toml`.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Explicit path to `mod.toml` or `Cargo.toml`.
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Build (or locate) the release artifact.
        #[arg(long)]
        release: bool,
        /// Output `.bampack` file path (defaults to `<project>/target/<profile>/<mod_id>-<version>.bampack`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Optional OpenSSH Ed25519 private key to sign the `.bampack` archive.
        #[arg(long)]
        key: Option<PathBuf>,
        /// Optional OpenSSH Ed25519 certificate (`*-cert.pub`) paired with `--key`.
        #[arg(long)]
        cert: Option<PathBuf>,
        /// `SSHSIG` namespace used when signing the `.bampack` archive.
        #[arg(long, default_value = pack::DEFAULT_BAMPACK_NAMESPACE)]
        namespace: String,
        /// Optional assets directory to bundle under `assets/` (defaults to `<project>/assets` if present).
        #[arg(long)]
        assets_dir: Option<PathBuf>,
        /// Skip building and locate an already-built shared library instead.
        #[arg(long)]
        skip_build: bool,
    },
    /// Watch guest mod sources/manifests, rebuild incrementally on change, sign, and atomically stage for hot-reload.
    Watch {
        /// Project directory containing `mod.toml` / `Cargo.toml`.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Explicit path to `mod.toml` or `Cargo.toml`.
        #[arg(long)]
        manifest_path: Option<PathBuf>,
        /// Build with release optimizations.
        #[arg(long)]
        release: bool,
        /// Optional staging root directory (`<out-dir>/<mod_id>/`) watched by `ModHotReloadPlugin`.
        #[arg(long)]
        out_dir: Option<PathBuf>,
        /// Optional OpenSSH Ed25519 private key to detach-sign every rebuilt shared library.
        #[arg(long)]
        key: Option<PathBuf>,
        /// `SSHSIG` namespace used when `--key` is passed.
        #[arg(long, default_value = signing::DEFAULT_NAMESPACE)]
        namespace: String,
        /// Polling interval in milliseconds for recursive source/manifest fingerprinting.
        #[arg(long, default_value_t = watch::DEFAULT_POLL_MS)]
        poll_ms: u64,
        /// Run a single build/sign/stage cycle and exit.
        #[arg(long, conflicts_with = "max_cycles")]
        once: bool,
        /// Stop after completing `<n>` build cycles (initial build is cycle `1`).
        #[arg(long)]
        max_cycles: Option<usize>,
    },
    /// Validate `mod.toml` manifests, SemVer dependencies, and topological load order in `<dir>`.
    CheckMods {
        /// Mods directory containing `mod.toml` or `<mod>/mod.toml` subdirectories.
        dir: PathBuf,
    },
    /// Diagnose host ABI `0.3.0` layout, Rust/MSVC/Clang/GCC/Zig toolchains, SDK header parity, and optional mods directory.
    Doctor {
        /// Optional mods directory to validate during the diagnostic pass.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// Fail if a C99 or C++17 compiler is not available on PATH.
        #[arg(long)]
        require_c: bool,
    },
    /// Generate an OpenSSH Ed25519 keypair (`<out>` + `<out>.pub`).
    Keygen {
        /// Output private key path prefix.
        #[arg(long)]
        out: PathBuf,
        /// Optional OpenSSH key comment.
        #[arg(long)]
        comment: Option<String>,
    },
    /// Sign a built guest mod shared library with an OpenSSH `SSHSIG` `.sig` sidecar.
    Sign {
        /// Signer OpenSSH Ed25519 private key.
        #[arg(long)]
        key: PathBuf,
        /// Optional explicit `.sig` output path (defaults to `<mod_path>.sig`).
        #[arg(long)]
        out: Option<PathBuf>,
        /// `SSHSIG` namespace.
        #[arg(long, default_value = signing::DEFAULT_NAMESPACE)]
        namespace: String,
        /// Path to the guest mod shared library.
        mod_path: PathBuf,
    },
    /// Verify a guest mod shared library (`.sig`/`.bam_sig`) or signed `.bampack` archive against trusted `.pub` or CA keys.
    Verify {
        /// Trusted OpenSSH Ed25519 `.pub` key(s).
        #[arg(long = "pubkey")]
        pubkeys: Vec<PathBuf>,
        /// Trusted OpenSSH Ed25519 Certificate Authority `.pub` key(s).
        #[arg(long = "ca")]
        ca_keys: Vec<PathBuf>,
        /// Optional binary OpenSSH Key Revocation List (`.krl`) file(s).
        #[arg(long = "krl")]
        krls: Vec<PathBuf>,
        /// Optional required certificate principal when verifying CA-signed artifacts.
        #[arg(long)]
        principal: Option<String>,
        /// Optional explicit `.sig` path (loose libraries only; defaults to `<mod_path>.sig`).
        #[arg(long)]
        sig: Option<PathBuf>,
        /// Expected `SSHSIG` namespace (defaults to `bevy_abi_mod.pack.v1` for `.bampack` or `bevy_abi_mod.v1` for libraries).
        #[arg(long)]
        namespace: Option<String>,
        /// Path to the guest mod shared library or `.bampack` archive.
        mod_path: PathBuf,
    },
}

fn normalize_cargo_subcommand_args<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let mut raw: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if raw.len() >= 2 && raw[1] == "bam" {
        raw.remove(1);
    }
    raw
}

/// Runs the `cargo-bam` CLI and returns a typed [`CliError`] on failure.
pub fn run_cli<I, T>(args: I) -> Result<(), CliError>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let normalized = normalize_cargo_subcommand_args(args);
    let cli = Cli::try_parse_from(normalized).map_err(|e| {
        if e.use_stderr() {
            CliError::InvalidArgument(e.to_string())
        } else {
            e.print().ok();
            CliError::InvalidArgument(String::new())
        }
    });

    let cli = match cli {
        Ok(c) => c,
        Err(CliError::InvalidArgument(msg)) if msg.is_empty() => return Ok(()),
        Err(e) => return Err(e),
    };

    match cli.command {
        Commands::New {
            name,
            lang,
            dir,
            guest_sdk_path,
        } => {
            let created = scaffold::scaffold_new_mod(&name, lang, &dir, guest_sdk_path.as_deref())?;
            println!("Created {lang} guest mod at {}", created.display());
            Ok(())
        }
        Commands::ExportSdk { out } => {
            let files = sdk::export_sdk(&out)?;
            println!("Exported {} SDK file(s) to {}", files.len(), out.display());
            Ok(())
        }
        Commands::Build {
            manifest_path,
            dir,
            release,
            out_dir,
            sign_key,
            sign_namespace,
        } => {
            let out = builder::build_mod_with_signing(
                manifest_path.as_deref(),
                dir.as_deref(),
                release,
                out_dir.as_deref(),
                sign_key.as_deref(),
                &sign_namespace,
            )?;
            println!(
                "Built {} mod library at {}",
                out.language,
                out.library_path.display()
            );
            if let Some(sig) = out.signature_path {
                println!("  signature: {}", sig.display());
            }
            Ok(())
        }
        Commands::Pack {
            dir,
            manifest_path,
            release,
            out,
            key,
            cert,
            namespace,
            assets_dir,
            skip_build,
        } => {
            let opts = pack::PackOptions {
                manifest_path,
                dir,
                release,
                out,
                key,
                cert,
                namespace,
                assets_dir,
                skip_build,
            };
            let res = pack::pack_mod(&opts)?;
            println!(
                "Packed {} mod '{} {}' ({} entries) -> {}",
                res.language,
                res.mod_id,
                res.version,
                res.entry_paths.len(),
                res.output_path.display()
            );
            println!("  sha256:    {}", res.archive_sha256);
            if let Some(fp) = res.signer_fingerprint {
                println!(
                    "  signed:    yes (signer {fp}, namespace {})",
                    res.namespace
                );
            } else {
                println!("  signed:    no (pass `--key <ed25519_key>` to sign)");
            }
            Ok(())
        }
        Commands::Watch {
            dir,
            manifest_path,
            release,
            out_dir,
            key,
            namespace,
            poll_ms,
            once,
            max_cycles,
        } => {
            let debounce_ms = poll_ms.min(watch::DEFAULT_DEBOUNCE_MS);
            let config = watch::WatchConfig {
                manifest_path,
                dir,
                release,
                out_dir,
                sign_key: key,
                namespace,
                poll_interval: Duration::from_millis(poll_ms),
                debounce: Duration::from_millis(debounce_ms),
                once,
                max_cycles,
            };
            let _cycles = watch::run_watch(config)?;
            Ok(())
        }
        Commands::CheckMods { dir } => {
            let order = dag::check_mods_dir(&dir)?;
            println!(
                "Validated {} mod manifest(s) in topological load order:",
                order.len()
            );
            for (idx, m) in order.iter().enumerate() {
                println!("  {}. {} {}", idx + 1, m.id, m.version);
            }
            Ok(())
        }
        Commands::Doctor { dir, require_c } => {
            let report = doctor::run_doctor(dir.as_deref(), require_c)?;
            println!(
                "cargo-bam doctor (host ABI {} / {}, target {}, {}-slot BamHostApi {} B)",
                report.host_abi_version,
                report.abi_version_hex,
                report.host_target_triple,
                report.host_api_slots,
                report.host_api_size
            );
            for check in &report.tools {
                let status = if check.available {
                    "ok"
                } else if check.required {
                    "MISSING"
                } else {
                    "absent (optional)"
                };
                println!("  [{status}] {}: {}", check.name, check.detail);
            }
            if let Some(mods) = &report.validated_mods {
                let names: Vec<String> = mods
                    .iter()
                    .map(|m| format!("{}@{}", m.id, m.version))
                    .collect();
                println!(
                    "  [ok] validated_mods: {} manifest(s) in DAG order ({})",
                    mods.len(),
                    names.join(" -> ")
                );
            }
            Ok(())
        }
        Commands::Keygen { out, comment } => {
            let res = signing::generate_keypair(&out, comment.as_deref())?;
            println!("Generated Ed25519 keypair: {}", res.private_key.display());
            println!("  public:      {}", res.public_key.display());
            println!("  fingerprint: {}", res.fingerprint);
            Ok(())
        }
        Commands::Sign {
            key,
            out,
            namespace,
            mod_path,
        } => {
            let res = signing::sign_mod_file(&key, &mod_path, out.as_deref(), &namespace)?;
            println!("Signed mod: {}", res.signature_path.display());
            println!("  signer_fp: {}", res.signer_fingerprint);
            println!("  sha256:    {}", res.binary_sha256);
            Ok(())
        }
        Commands::Verify {
            pubkeys,
            ca_keys,
            krls,
            principal,
            sig,
            namespace,
            mod_path,
        } => {
            if pack::is_bampack_file(&mod_path) {
                if sig.is_some() {
                    return Err(CliError::InvalidArgument(
                        "`--sig` is only valid for detached shared-library signatures, not `.bampack` archives"
                            .to_owned(),
                    ));
                }
                let ns = namespace
                    .as_deref()
                    .unwrap_or(pack::DEFAULT_BAMPACK_NAMESPACE);
                let verified = pack::verify_bampack_file_with_krl(
                    &mod_path,
                    &pubkeys,
                    &ca_keys,
                    &krls,
                    principal.as_deref(),
                    ns,
                )?;
                println!(
                    "Verified .bampack archive (signer {}, {} entries)",
                    verified.signature.signer_fingerprint,
                    verified.entries.len()
                );
                Ok(())
            } else {
                let ns = namespace.as_deref().unwrap_or(signing::DEFAULT_NAMESPACE);
                let fp = signing::verify_mod_file_with_trust(
                    &mod_path,
                    sig.as_deref(),
                    &pubkeys,
                    &ca_keys,
                    &krls,
                    principal.as_deref(),
                    ns,
                )?;
                println!("Verified mod signature (signer {fp})");
                Ok(())
            }
        }
    }
}

/// Executes the `cargo-bam` CLI with the provided raw command-line arguments.
pub fn run_from_args<I, T>(args: I) -> ExitCode
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    match run_cli(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("cargo-bam error: {err}");
            ExitCode::FAILURE
        }
    }
}
