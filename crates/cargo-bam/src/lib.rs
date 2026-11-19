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

use bevy_abi_mod_trust::ModTrustPolicy;
use clap::{Parser, Subcommand};
use dag::ValidatedModManifest;
use scaffold::ModLanguage;
use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
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

fn detect_rust_crate_stem(cargo_toml: &Path) -> Result<String, CliError> {
    let content = fs::read_to_string(cargo_toml)?;
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
    Ok(name.replace('-', "_"))
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

    let mod_id = manifest.as_ref().map(|m| m.id.clone()).unwrap_or_else(|| {
        project_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("bam_mod")
            .to_owned()
    });

    let lib_stem = if language == ModLanguage::Rust && cargo_toml.is_file() {
        detect_rust_crate_stem(&cargo_toml)?
    } else {
        manifest
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
            .replace('-', "_")
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

fn verify_loose_mod_with_trust(
    mod_path: &Path,
    sig_path: Option<&Path>,
    pubkeys: &[PathBuf],
    ca_keys: &[PathBuf],
    principal: Option<&str>,
    namespace: &str,
) -> Result<String, CliError> {
    if ca_keys.is_empty() && principal.is_none() {
        let fp = signing::verify_mod_file(mod_path, sig_path, pubkeys, namespace)?;
        return Ok(fp.to_string());
    }
    if pubkeys.is_empty() && ca_keys.is_empty() {
        return Err(CliError::InvalidArgument(
            "at least one `--pubkey <key.pub>` or `--ca <ca.pub>` is required".to_owned(),
        ));
    }
    let mut policy = ModTrustPolicy::strict()
        .with_namespace(namespace)
        .map_err(|e| CliError::Crypto(e.to_string()))?;
    for pk in pubkeys {
        policy = policy
            .add_public_key_file(pk)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    for ca in ca_keys {
        policy = policy
            .add_ca_file(ca)
            .map_err(|e| CliError::Crypto(e.to_string()))?;
    }
    if let Some(req) = principal {
        policy = policy.with_required_principal(req);
    }

    let binary_bytes = fs::read(mod_path)?;
    if binary_bytes.is_empty() {
        return Err(CliError::Crypto("mod binary is empty".to_owned()));
    }
    let receipt = match sig_path {
        Some(explicit_sig) => {
            let sig_bytes = fs::read(explicit_sig)?;
            policy
                .verify_bytes(&binary_bytes, &sig_bytes)
                .map_err(|e| CliError::Crypto(e.to_string()))?
        }
        None => {
            let default_sig = signing::default_sig_path(mod_path);
            if default_sig.is_file() {
                let sig_bytes = fs::read(&default_sig)?;
                policy
                    .verify_bytes(&binary_bytes, &sig_bytes)
                    .map_err(|e| CliError::Crypto(e.to_string()))?
            } else {
                policy
                    .verify_embedded_bam_sig(&binary_bytes)
                    .map_err(|e| CliError::Crypto(e.to_string()))?
            }
        }
    };
    Ok(receipt.signer_fingerprint)
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
                let verified = pack::verify_bampack_file(
                    &mod_path,
                    &pubkeys,
                    &ca_keys,
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
                let fp = verify_loose_mod_with_trust(
                    &mod_path,
                    sig.as_deref(),
                    &pubkeys,
                    &ca_keys,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn cli_export_sdk_writes_c_cpp_and_zig_headers() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("sdk");

        run_cli([
            "cargo-bam",
            "export-sdk",
            "--out",
            out_dir.to_str().unwrap(),
        ])
        .unwrap();
        assert!(out_dir.join("bevy_abi_mod.h").is_file());
        assert!(out_dir.join("bevy_abi_mod.hpp").is_file());
        assert!(out_dir.join("bevy_abi_mod.zig").is_file());

        let h = fs::read_to_string(out_dir.join("bevy_abi_mod.h")).unwrap();
        assert!(h.contains("BAM_ABI_VERSION"));
    }

    #[test]
    fn cli_new_build_keygen_sign_and_verify_c99_and_cpp17_mods() {
        let tmp = TempDir::new().unwrap();

        // 1. Test `cargo bam new` (Cargo subcommand prefix stripping) for C99 & C++17
        run_cli([
            "cargo-bam",
            "bam",
            "new",
            "sample_c_mod",
            "--lang",
            "c99",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap();
        let c_proj = tmp.path().join("sample_c_mod");
        assert!(c_proj.join("mod.toml").is_file());
        assert!(c_proj.join("src/mod.c").is_file());

        // 2. Build C99 mod & stage into `dist/sample_c_mod/`
        let dist_dir = tmp.path().join("dist_c");
        run_cli([
            "cargo-bam",
            "build",
            "--manifest-path",
            c_proj.join("mod.toml").to_str().unwrap(),
            "--release",
            "--out-dir",
            dist_dir.to_str().unwrap(),
        ])
        .unwrap();
        let staged_c_dir = dist_dir.join("sample_c_mod");
        assert!(staged_c_dir.join("mod.toml").is_file());
        let staged_c_lib = fs::read_dir(&staged_c_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
            })
            .expect("staged C99 shared library must exist");

        // 3. Keygen, sign, and verify the staged C99 mod binary
        let key_prefix = tmp.path().join("keys/dev_signer");
        run_cli([
            "cargo-bam",
            "keygen",
            "--out",
            key_prefix.to_str().unwrap(),
            "--comment",
            "ci-test",
        ])
        .unwrap();
        assert!(key_prefix.is_file());
        let pub_path = tmp.path().join("keys/dev_signer.pub");
        assert!(pub_path.is_file());

        run_cli([
            "cargo-bam",
            "sign",
            "--key",
            key_prefix.to_str().unwrap(),
            staged_c_lib.to_str().unwrap(),
        ])
        .unwrap();
        let sig_path = signing::default_sig_path(&staged_c_lib);
        assert!(sig_path.is_file());

        run_cli([
            "cargo-bam",
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            staged_c_lib.to_str().unwrap(),
        ])
        .unwrap();

        // Tampering the binary must fail verification!
        let mut tampered = fs::read(&staged_c_lib).unwrap();
        tampered[0] ^= 0xff;
        fs::write(&staged_c_lib, tampered).unwrap();
        assert!(
            run_cli([
                "cargo-bam",
                "verify",
                "--pubkey",
                pub_path.to_str().unwrap(),
                staged_c_lib.to_str().unwrap(),
            ])
            .is_err()
        );

        // 4. Scaffold & build C++17 mod with `--sign-key` staged into `dist_cpp/sample_cpp_mod/`
        run_cli([
            "cargo-bam",
            "new",
            "sample_cpp_mod",
            "--lang",
            "cpp17",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap();
        let cpp_proj = tmp.path().join("sample_cpp_mod");
        let dist_cpp = tmp.path().join("dist_cpp");
        run_cli([
            "cargo-bam",
            "build",
            "--dir",
            cpp_proj.to_str().unwrap(),
            "--out-dir",
            dist_cpp.to_str().unwrap(),
            "--sign-key",
            key_prefix.to_str().unwrap(),
        ])
        .unwrap();
        let staged_cpp_dir = dist_cpp.join("sample_cpp_mod");
        assert!(staged_cpp_dir.join("mod.toml").is_file());
        let staged_cpp_lib = fs::read_dir(&staged_cpp_dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| {
                p.extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| matches!(e, "so" | "dylib" | "dll"))
            })
            .expect("staged C++17 shared library must exist");
        assert!(signing::default_sig_path(&staged_cpp_lib).is_file());
        run_cli([
            "cargo-bam",
            "verify",
            "--pubkey",
            pub_path.to_str().unwrap(),
            staged_cpp_lib.to_str().unwrap(),
        ])
        .unwrap();
    }

    #[test]
    fn cli_new_and_build_rust_cdylib_mod() {
        let tmp = TempDir::new().unwrap();
        run_cli([
            "cargo-bam",
            "new",
            "sample_rust_mod",
            "--lang",
            "rust",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap();

        let rust_proj = tmp.path().join("sample_rust_mod");
        assert!(rust_proj.join("mod.toml").is_file());
        assert!(rust_proj.join("Cargo.toml").is_file());
        assert!(rust_proj.join("src/lib.rs").is_file());

        let dist_rust = tmp.path().join("dist_rust");
        run_cli([
            "cargo-bam",
            "build",
            "--dir",
            rust_proj.to_str().unwrap(),
            "--out-dir",
            dist_rust.to_str().unwrap(),
        ])
        .unwrap();
        let staged_rust_dir = dist_rust.join("sample_rust_mod");
        assert!(staged_rust_dir.join("mod.toml").is_file());
        let has_lib = fs::read_dir(&staged_rust_dir).unwrap().flatten().any(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "so" | "dylib" | "dll"))
        });
        assert!(
            has_lib,
            "built Rust cdylib must be staged into dist_rust/<mod_id>/"
        );
    }

    #[test]
    fn cli_check_mods_validates_dag_semver_mismatch_and_cycles() {
        let tmp = TempDir::new().unwrap();
        let mods_dir = tmp.path().join("mods");
        fs::create_dir_all(mods_dir.join("base")).unwrap();
        fs::create_dir_all(mods_dir.join("addon")).unwrap();
        fs::create_dir_all(mods_dir.join("ui")).unwrap();

        fs::write(
            mods_dir.join("base/mod.toml"),
            "[mod]\nid = \"base\"\nversion = \"1.2.0\"\nabi_version = \"^0.3.0\"\n",
        )
        .unwrap();
        fs::write(
            mods_dir.join("addon/mod.toml"),
            "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\nabi_version = \"^0.3.0\"\n\n[dependencies]\nbase = \"^1.0\"\n",
        )
        .unwrap();
        fs::write(
            mods_dir.join("ui/mod.toml"),
            "[mod]\nid = \"ui\"\nversion = \"0.5.0\"\n\n[dependencies]\naddon = \"^1.0\"\n",
        )
        .unwrap();

        // Valid DAG returns topological order: base -> addon -> ui
        let order = dag::check_mods_dir(&mods_dir).unwrap();
        let ids: Vec<&str> = order.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["base", "addon", "ui"]);
        run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap();

        // SemVer mismatch must fail!
        fs::write(
            mods_dir.join("addon/mod.toml"),
            "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase = \"^2.0\"\n",
        )
        .unwrap();
        let err = run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap_err();
        assert!(
            err.to_string().contains("requires 'base' ^2.0"),
            "unexpected error: {err}"
        );

        // Cycle must fail!
        fs::write(
            mods_dir.join("addon/mod.toml"),
            "[mod]\nid = \"addon\"\nversion = \"1.0.0\"\n\n[dependencies]\nbase = \"^1.0\"\nui = \"^0.5\"\n",
        )
        .unwrap();
        let cyc_err = run_cli(["cargo-bam", "check-mods", mods_dir.to_str().unwrap()]).unwrap_err();
        assert!(
            cyc_err.to_string().contains("dependency cycle"),
            "unexpected error: {cyc_err}"
        );
    }

    #[test]
    fn cli_new_scaffolds_zig_mod() {
        let tmp = TempDir::new().unwrap();
        run_cli([
            "cargo-bam",
            "new",
            "sample_zig_mod",
            "--lang",
            "zig",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap();
        let zig_proj = tmp.path().join("sample_zig_mod");
        assert!(zig_proj.join("mod.toml").is_file());
        assert!(zig_proj.join("build.zig").is_file());
        assert!(zig_proj.join("src/root.zig").is_file());
        assert!(zig_proj.join("include/bevy_abi_mod.zig").is_file());
    }

    #[test]
    fn cli_header_alias_and_doctor_validate_environment_and_mods() {
        let tmp = TempDir::new().unwrap();
        let out_dir = tmp.path().join("headers");
        run_cli(["cargo", "bam", "header", "--out", out_dir.to_str().unwrap()]).unwrap();
        assert!(out_dir.join("bevy_abi_mod.h").is_file());
        assert!(out_dir.join("bevy_abi_mod.hpp").is_file());
        assert!(out_dir.join("bevy_abi_mod.zig").is_file());

        let mods_dir = tmp.path().join("mods");
        fs::create_dir_all(mods_dir.join("core")).unwrap();
        fs::write(
            mods_dir.join("core/mod.toml"),
            "[mod]\nid = \"core\"\nversion = \"0.2.0\"\nabi_version = \"^0.3.0\"\n",
        )
        .unwrap();

        run_cli(["cargo-bam", "doctor", "--dir", mods_dir.to_str().unwrap()]).unwrap();

        let report = doctor::run_doctor(Some(&mods_dir), false).unwrap();
        assert!(report.ok);
        assert_eq!(report.host_abi_version, "0.3.0");
        assert!(
            report
                .tools
                .iter()
                .any(|t| t.name == "cargo" && t.available)
        );
        assert!(
            report
                .tools
                .iter()
                .any(|t| t.name == "rustc" && t.available)
        );
        assert!(
            report
                .tools
                .iter()
                .any(|t| t.name == "sdk_headers" && t.available)
        );
        assert_eq!(report.validated_mods.as_ref().map(Vec::len), Some(1));

        // Corrupt ABI version in mod.toml must fail doctor --dir
        fs::write(
            mods_dir.join("core/mod.toml"),
            "[mod]\nid = \"core\"\nversion = \"0.2.0\"\nabi_version = \"^99.0.0\"\n",
        )
        .unwrap();
        let err =
            run_cli(["cargo-bam", "doctor", "--dir", mods_dir.to_str().unwrap()]).unwrap_err();
        assert!(
            err.to_string().contains("0.3.0"),
            "expected ABI compatibility error mentioning 0.3.0, got: {err}"
        );
    }

    #[test]
    fn cli_rejects_missing_sign_key_and_invalid_mod_id() {
        let tmp = TempDir::new().unwrap();
        let invalid_id_err = run_cli([
            "cargo-bam",
            "new",
            "../escape",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap_err();
        assert!(
            matches!(invalid_id_err, CliError::InvalidArgument(_)),
            "expected InvalidArgument, got {invalid_id_err:?}"
        );

        let proj = tmp.path().join("c_sample");
        run_cli([
            "cargo-bam",
            "new",
            "c_sample",
            "--lang",
            "c99",
            "--dir",
            tmp.path().to_str().unwrap(),
        ])
        .unwrap();
        let missing_key = tmp.path().join("does_not_exist.key");
        let build_err = run_cli([
            "cargo-bam",
            "build",
            "--dir",
            proj.to_str().unwrap(),
            "--sign-key",
            missing_key.to_str().unwrap(),
        ])
        .unwrap_err();
        assert!(
            matches!(build_err, CliError::Io(_) | CliError::Crypto(_)),
            "expected missing key error, got {build_err:?}"
        );
    }
}
