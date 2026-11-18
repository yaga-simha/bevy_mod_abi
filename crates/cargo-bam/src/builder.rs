//! Mod build and staging (`cargo bam build`) for Rust, C99, C++17, and Zig guest mods.

use crate::CliError;
use crate::dag::ValidatedModManifest;
use crate::scaffold::ModLanguage;
use crate::signing::{self, DEFAULT_NAMESPACE};
use serde::Deserialize;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Result returned by [`build_mod`].
#[derive(Debug, Clone)]
pub struct BuildOutput {
    /// Detected guest language.
    pub language: ModLanguage,
    /// Built shared library path (staged if `--out-dir` was provided, otherwise build output).
    pub library_path: PathBuf,
    /// Staged `mod.toml` path if `mod.toml` existed and `--out-dir` was provided.
    pub staged_manifest_path: Option<PathBuf>,
    /// Detached `.sig` sidecar path if `--sign-key` was provided.
    pub signature_path: Option<PathBuf>,
}

fn dylib_naming() -> (&'static str, &'static str) {
    if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    }
}

fn c_compiler() -> String {
    env::var("CC")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "gcc".to_owned()
            } else {
                "cc".to_owned()
            }
        })
}

fn cxx_compiler() -> String {
    env::var("CXX")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "g++".to_owned()
            } else {
                "c++".to_owned()
            }
        })
}

fn cargo_bin() -> String {
    env::var("CARGO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cargo".to_owned())
}

fn run_command(cmd: &mut Command, label: &str) -> Result<(), CliError> {
    let output = cmd
        .output()
        .map_err(|e| CliError::Command(format!("failed to launch `{label}`: {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(CliError::Command(format!(
            "`{label}` exited with {}\n{stdout}\n{stderr}",
            output.status
        )));
    }
    Ok(())
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

fn resolve_project(
    manifest_path: Option<&Path>,
    dir: Option<&Path>,
) -> Result<(PathBuf, Option<PathBuf>, Option<ValidatedModManifest>), CliError> {
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

    let parsed_mod_toml = if let Some(ref mp) = mod_toml_path {
        Some(ValidatedModManifest::from_file(mp)?)
    } else {
        None
    };

    Ok((project_dir, mod_toml_path, parsed_mod_toml))
}

fn find_first_existing(project_dir: &Path, candidates: &[&str]) -> Option<PathBuf> {
    candidates
        .iter()
        .map(|rel| project_dir.join(rel))
        .find(|p| p.is_file())
}

/// Builds a guest mod project (Rust, C99, C++17, or Zig), validates `mod.toml` metadata if
/// present, optionally stages into `out_dir/<mod_id>/` ready for `load_mods_dir`, and optionally
/// signs the artifact with `sign_key`.
pub fn build_mod(
    manifest_path: Option<&Path>,
    dir: Option<&Path>,
    release: bool,
    out_dir: Option<&Path>,
) -> Result<BuildOutput, CliError> {
    build_mod_with_signing(
        manifest_path,
        dir,
        release,
        out_dir,
        None,
        DEFAULT_NAMESPACE,
    )
}

/// Builds, stages into `out_dir/<mod_id>/`, and optionally signs a guest mod artifact.
pub fn build_mod_with_signing(
    manifest_path: Option<&Path>,
    dir: Option<&Path>,
    release: bool,
    out_dir: Option<&Path>,
    sign_key: Option<&Path>,
    sign_namespace: &str,
) -> Result<BuildOutput, CliError> {
    let (project_dir, mod_toml_path, parsed_mod) = resolve_project(manifest_path, dir)?;
    let cargo_toml = project_dir.join("Cargo.toml");

    let explicit_lang = parsed_mod
        .as_ref()
        .and_then(|m| m.language.as_deref())
        .and_then(parse_language_str);

    let language = if let Some(lang) = explicit_lang {
        lang
    } else if cargo_toml.is_file() {
        ModLanguage::Rust
    } else if project_dir.join("build.zig").is_file() {
        ModLanguage::Zig
    } else if find_first_existing(&project_dir, &["src/mod.cpp", "mod.cpp"]).is_some() {
        ModLanguage::Cpp17
    } else if find_first_existing(&project_dir, &["src/mod.c", "mod.c"]).is_some() {
        ModLanguage::C99
    } else {
        return Err(CliError::InvalidManifest(format!(
            "could not determine mod language in {} (missing `mod.toml`, `Cargo.toml`, `build.zig`, `src/mod.c`, or `src/mod.cpp`)",
            project_dir.display()
        )));
    };

    let mod_id = parsed_mod
        .as_ref()
        .map(|m| m.id.clone())
        .unwrap_or_else(|| {
            project_dir
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("bam_mod")
                .to_owned()
        });
    let stem_from_mod = parsed_mod
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

    let (prefix, ext) = dylib_naming();
    let built_artifact = match language {
        ModLanguage::Rust => {
            if !cargo_toml.is_file() {
                return Err(CliError::InvalidManifest(format!(
                    "expected Cargo.toml at {}",
                    cargo_toml.display()
                )));
            }
            let crate_stem = detect_rust_crate_stem(&cargo_toml)?;
            let mut cmd = Command::new(cargo_bin());
            cmd.current_dir(&project_dir)
                .arg("build")
                .arg("--manifest-path")
                .arg(&cargo_toml);
            if release {
                cmd.arg("--release");
            }
            let profile_dir = if release { "release" } else { "debug" };
            run_command(&mut cmd, &format!("cargo build ({profile_dir})"))?;
            let target_root = env::var("CARGO_TARGET_DIR")
                .ok()
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| project_dir.join("target"));
            let artifact = target_root
                .join(profile_dir)
                .join(format!("{prefix}{crate_stem}.{ext}"));
            if !artifact.is_file() {
                return Err(CliError::Command(format!(
                    "expected Rust cdylib artifact not found at {}",
                    artifact.display()
                )));
            }
            artifact
        }
        ModLanguage::C99 => {
            let src =
                find_first_existing(&project_dir, &["src/mod.c", "mod.c"]).ok_or_else(|| {
                    CliError::InvalidManifest(format!(
                        "expected C99 source `src/mod.c` or `mod.c` in {}",
                        project_dir.display()
                    ))
                })?;
            let build_dir =
                project_dir
                    .join("target")
                    .join(if release { "release" } else { "debug" });
            fs::create_dir_all(&build_dir)?;
            let out_path = build_dir.join(format!("{prefix}{stem_from_mod}.{ext}"));
            let include_dir = project_dir.join("include");
            let cc = c_compiler();
            let mut cmd = Command::new(&cc);
            cmd.current_dir(&project_dir)
                .args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
                .arg(if release { "-O2" } else { "-O0" })
                .arg("-shared");
            if include_dir.is_dir() {
                cmd.arg(format!("-I{}", include_dir.display()));
            }
            if !cfg!(target_os = "windows") {
                cmd.arg("-fPIC");
            }
            cmd.arg(&src).arg("-o").arg(&out_path);
            run_command(&mut cmd, &format!("{cc} -std=c99"))?;
            out_path
        }
        ModLanguage::Cpp17 => {
            let src = find_first_existing(&project_dir, &["src/mod.cpp", "mod.cpp"]).ok_or_else(
                || {
                    CliError::InvalidManifest(format!(
                        "expected C++17 source `src/mod.cpp` or `mod.cpp` in {}",
                        project_dir.display()
                    ))
                },
            )?;
            let build_dir =
                project_dir
                    .join("target")
                    .join(if release { "release" } else { "debug" });
            fs::create_dir_all(&build_dir)?;
            let out_path = build_dir.join(format!("{prefix}{stem_from_mod}.{ext}"));
            let include_dir = project_dir.join("include");
            let cxx = cxx_compiler();
            let mut cmd = Command::new(&cxx);
            cmd.current_dir(&project_dir)
                .args(["-std=c++17", "-Wall", "-Wextra", "-Wpedantic", "-Werror"])
                .arg(if release { "-O2" } else { "-O0" })
                .arg("-shared");
            if include_dir.is_dir() {
                cmd.arg(format!("-I{}", include_dir.display()));
            }
            if !cfg!(target_os = "windows") {
                cmd.args([
                    "-fPIC",
                    "-fvisibility=hidden",
                    "-fvisibility-inlines-hidden",
                ]);
            }
            cmd.arg(&src).arg("-o").arg(&out_path);
            run_command(&mut cmd, &format!("{cxx} -std=c++17"))?;
            out_path
        }
        ModLanguage::Zig => {
            let mut cmd = Command::new("zig");
            cmd.current_dir(&project_dir).arg("build");
            if release {
                cmd.arg("-Doptimize=ReleaseFast");
            }
            run_command(&mut cmd, "zig build")?;
            let expected_name = format!("{prefix}{stem_from_mod}.{ext}");
            let candidates = [
                project_dir.join("zig-out").join("lib").join(&expected_name),
                project_dir.join("zig-out").join("bin").join(&expected_name),
            ];
            candidates
                .into_iter()
                .find(|p| p.is_file())
                .ok_or_else(|| {
                    CliError::Command(format!(
                        "expected Zig shared library `{expected_name}` under zig-out/"
                    ))
                })?
        }
    };

    if let Some(stage_root) = out_dir {
        let stage_dir = stage_root.join(&mod_id);
        fs::create_dir_all(&stage_dir)?;
        let file_name = built_artifact
            .file_name()
            .ok_or_else(|| CliError::Command("built artifact has no filename".to_owned()))?;
        let staged_lib = stage_dir.join(file_name);
        fs::copy(&built_artifact, &staged_lib)?;
        let staged_manifest = if let Some(ref mp) = mod_toml_path {
            let dest_manifest = stage_dir.join("mod.toml");
            fs::copy(mp, &dest_manifest)?;
            Some(dest_manifest)
        } else {
            None
        };
        let signature_path = if let Some(key_path) = sign_key {
            let signed = signing::sign_mod_file(key_path, &staged_lib, None, sign_namespace)?;
            Some(signed.signature_path)
        } else {
            None
        };
        return Ok(BuildOutput {
            language,
            library_path: staged_lib,
            staged_manifest_path: staged_manifest,
            signature_path,
        });
    }

    let signature_path = if let Some(key_path) = sign_key {
        let signed = signing::sign_mod_file(key_path, &built_artifact, None, sign_namespace)?;
        Some(signed.signature_path)
    } else {
        None
    };

    Ok(BuildOutput {
        language,
        library_path: built_artifact,
        staged_manifest_path: None,
        signature_path,
    })
}
