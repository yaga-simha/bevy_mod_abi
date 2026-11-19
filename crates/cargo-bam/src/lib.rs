#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Library implementation for the `cargo-bam` modding CLI toolchain (`ABI 0.2.0`).

pub mod builder;
pub mod dag;
pub mod doctor;
pub mod scaffold;
pub mod sdk;
pub mod signing;

use clap::{Parser, Subcommand};
use scaffold::ModLanguage;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
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

#[derive(Debug, Parser)]
#[command(
    name = "cargo-bam",
    bin_name = "cargo bam",
    version,
    about = "Build, scaffold, export SDK headers, and sign bevy_abi_mod guest mods"
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
    /// Validate `mod.toml` manifests, SemVer dependencies, and topological load order in `<dir>`.
    CheckMods {
        /// Mods directory containing `mod.toml` or `<mod>/mod.toml` subdirectories.
        dir: PathBuf,
    },
    /// Diagnose host ABI, Rust/C99/C++17/Zig toolchains, embedded SDK headers, and optional mods directory.
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
    /// Verify a guest mod shared library against a detached `.sig` sidecar and pinned `.pub` keys.
    Verify {
        /// Trusted OpenSSH Ed25519 `.pub` key(s).
        #[arg(long = "pubkey", required = true)]
        pubkeys: Vec<PathBuf>,
        /// Optional explicit `.sig` path (defaults to `<mod_path>.sig`).
        #[arg(long)]
        sig: Option<PathBuf>,
        /// Expected `SSHSIG` namespace.
        #[arg(long, default_value = signing::DEFAULT_NAMESPACE)]
        namespace: String,
        /// Path to the guest mod shared library.
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
            println!("cargo-bam doctor (host ABI {})", report.host_abi_version);
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
                println!(
                    "  [ok] validated_mods: {} manifest(s) in DAG order",
                    mods.len()
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
            sig,
            namespace,
            mod_path,
        } => {
            let fp = signing::verify_mod_file(&mod_path, sig.as_deref(), &pubkeys, &namespace)?;
            println!("Verified mod signature (signer {fp})");
            Ok(())
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
