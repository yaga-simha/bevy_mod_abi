#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! Library implementation for the `cargo-bam` modding CLI toolchain (`ABI 0.2.0`).

pub mod builder;
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
    ExportSdk {
        /// Output directory for exported headers.
        #[arg(long)]
        out: PathBuf,
    },
    /// Build a guest mod shared library and optionally stage it alongside `mod.toml`.
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
        /// Optional staging directory for built shared library + `mod.toml`.
        #[arg(long)]
        out_dir: Option<PathBuf>,
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
        } => {
            let out = builder::build_mod(
                manifest_path.as_deref(),
                dir.as_deref(),
                release,
                out_dir.as_deref(),
            )?;
            println!(
                "Built {} mod library at {}",
                out.language,
                out.library_path.display()
            );
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

        // 2. Build C99 mod & stage into `dist/`
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
        assert!(dist_dir.join("mod.toml").is_file());
        let staged_c_lib = fs::read_dir(&dist_dir)
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

        // 4. Scaffold & build C++17 mod
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
        ])
        .unwrap();
        assert!(dist_cpp.join("mod.toml").is_file());
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
        assert!(dist_rust.join("mod.toml").is_file());
        let has_lib = fs::read_dir(&dist_rust).unwrap().flatten().any(|e| {
            e.path()
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| matches!(ext, "so" | "dylib" | "dll"))
        });
        assert!(has_lib, "built Rust cdylib must be staged into dist_rust");
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
}
