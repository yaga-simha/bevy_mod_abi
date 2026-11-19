//! Mod build and staging (`cargo bam build`) for Rust, C99, C++17, and Zig guest mods.

use crate::CliError;
use crate::dag::ValidatedModManifest;
use crate::scaffold::ModLanguage;
use crate::signing::{self, DEFAULT_NAMESPACE};
use serde::Deserialize;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(windows)]
use std::sync::OnceLock;

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

pub(crate) fn is_msvc_style_driver(tool: &str) -> bool {
    let file_name = Path::new(tool)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("cl") || stem.eq_ignore_ascii_case("clang-cl")
}

pub(crate) fn is_plain_cl_driver(tool: &str) -> bool {
    let file_name = Path::new(tool)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(tool);
    let stem = file_name
        .strip_suffix(".exe")
        .or_else(|| file_name.strip_suffix(".EXE"))
        .unwrap_or(file_name);
    stem.eq_ignore_ascii_case("cl")
}

fn msvc_path_arg(prefix: &str, path: &Path) -> OsString {
    let mut arg = OsString::from(prefix);
    arg.push(path.as_os_str());
    arg
}

#[cfg_attr(not(windows), allow(dead_code))]
fn is_msvc_env_key(key: &str) -> bool {
    let upper = key.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "PATH"
            | "INCLUDE"
            | "EXTERNAL_INCLUDE"
            | "LIB"
            | "LIBPATH"
            | "PLATFORM"
            | "DEVENVDIR"
            | "VISUALSTUDIOVERSION"
            | "PREFERREDTOOLARCHITECTURE"
            | "UCRTVERSION"
            | "UNIVERSALCRTSDKDIR"
            | "VCINSTALLDIR"
            | "VSINSTALLDIR"
            | "NETFXSDKDIR"
    ) || upper.starts_with("VSCMD_")
        || upper.starts_with("VCTOOLS")
        || upper.starts_with("WINDOWSSDK")
}

#[cfg_attr(not(windows), allow(dead_code))]
fn parse_vcvars_env_output(stdout: &str) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = Vec::new();
    for raw_line in stdout.lines() {
        let line = raw_line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('=') {
            continue;
        }
        let Some((raw_key, value)) = line.split_once('=') else {
            continue;
        };
        let key = raw_key.trim();
        if key.is_empty() || !is_msvc_env_key(key) {
            continue;
        }
        let normalized_key = if key.eq_ignore_ascii_case("PATH") {
            "PATH".to_owned()
        } else if key.eq_ignore_ascii_case("INCLUDE") {
            "INCLUDE".to_owned()
        } else if key.eq_ignore_ascii_case("LIB") {
            "LIB".to_owned()
        } else if key.eq_ignore_ascii_case("LIBPATH") {
            "LIBPATH".to_owned()
        } else {
            key.to_owned()
        };
        if let Some(existing) = vars
            .iter_mut()
            .find(|(k, _)| k.eq_ignore_ascii_case(&normalized_key))
        {
            existing.1 = value.to_owned();
        } else {
            vars.push((normalized_key, value.to_owned()));
        }
    }
    vars
}

#[cfg_attr(not(windows), allow(dead_code))]
fn msvc_vcvars_bat_candidates(
    install_root: &Path,
    arch: &str,
) -> Vec<(PathBuf, Option<&'static str>)> {
    let aux_build = install_root.join("VC").join("Auxiliary").join("Build");
    match arch {
        "x86" => vec![
            (aux_build.join("vcvars32.bat"), None),
            (aux_build.join("vcvarsall.bat"), Some("x86")),
        ],
        "aarch64" => vec![
            (aux_build.join("vcvarsall.bat"), Some("arm64")),
            (aux_build.join("vcvars64.bat"), None),
        ],
        _ => vec![
            (aux_build.join("vcvars64.bat"), None),
            (aux_build.join("vcvarsall.bat"), Some("x64")),
        ],
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
fn find_exe_in_path_list(exe_name: &str, path_value: &str) -> Option<PathBuf> {
    let has_exe_ext = Path::new(exe_name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("exe"));
    for dir in env::split_paths(path_value) {
        let direct = dir.join(exe_name);
        if direct.is_file() {
            return Some(direct);
        }
        if !has_exe_ext {
            let with_exe = dir.join(format!("{exe_name}.exe"));
            if with_exe.is_file() {
                return Some(with_exe);
            }
        }
    }
    None
}

#[cfg(windows)]
#[derive(Clone, Debug)]
struct MsvcEnv {
    vars: Vec<(String, String)>,
    cl_exe: Option<PathBuf>,
}

#[cfg(windows)]
fn find_vs_install_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut vswhere_bins = Vec::new();
    for env_key in ["ProgramFiles(x86)", "ProgramFiles"] {
        if let Some(base) = env::var_os(env_key) {
            let candidate = PathBuf::from(base)
                .join("Microsoft Visual Studio")
                .join("Installer")
                .join("vswhere.exe");
            if candidate.is_file() && !vswhere_bins.contains(&candidate) {
                vswhere_bins.push(candidate);
            }
        }
    }
    vswhere_bins.push(PathBuf::from("vswhere.exe"));

    let required_component = if cfg!(target_arch = "aarch64") {
        "Microsoft.VisualStudio.Component.VC.Tools.ARM64"
    } else {
        "Microsoft.VisualStudio.Component.VC.Tools.x86.x64"
    };
    let queries: [&[&str]; 2] = [
        &[
            "-latest",
            "-products",
            "*",
            "-requires",
            required_component,
            "-property",
            "installationPath",
        ],
        &[
            "-latest",
            "-products",
            "*",
            "-prerelease",
            "-requires",
            required_component,
            "-property",
            "installationPath",
        ],
    ];

    for vswhere in &vswhere_bins {
        for query in &queries {
            let Ok(out) = Command::new(vswhere).args(*query).output() else {
                continue;
            };
            if !out.status.success() {
                continue;
            }
            let stdout = String::from_utf8_lossy(&out.stdout);
            for line in stdout.lines() {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    let path = PathBuf::from(trimmed);
                    if path.is_dir() && !roots.contains(&path) {
                        roots.push(path);
                    }
                }
            }
            if !roots.is_empty() {
                return roots;
            }
        }
    }

    if let Some(vs_dir) = env::var_os("VSINSTALLDIR") {
        let path = PathBuf::from(vs_dir);
        if path.is_dir() && !roots.contains(&path) {
            roots.push(path);
        }
    }
    for env_key in ["ProgramFiles", "ProgramFiles(x86)"] {
        let Some(base) = env::var_os(env_key) else {
            continue;
        };
        for year in ["2022", "2019"] {
            for edition in ["Enterprise", "Professional", "Community", "BuildTools"] {
                let cand = PathBuf::from(&base)
                    .join("Microsoft Visual Studio")
                    .join(year)
                    .join(edition);
                if cand.is_dir() && !roots.contains(&cand) {
                    roots.push(cand);
                }
            }
        }
    }

    roots
}

#[cfg(windows)]
fn capture_vcvars_env(bat_path: &Path, bat_arg: Option<&str>) -> Option<MsvcEnv> {
    use std::os::windows::process::CommandExt;

    let comspec = env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"));
    let mut cmd = Command::new(comspec);
    cmd.env("VSCMD_SKIP_SENDTELEMETRY", "1")
        .env("BAM_VCVARS_BAT", bat_path)
        .args(["/D", "/S", "/C"]);
    match bat_arg {
        Some(arg) => {
            cmd.raw_arg(format!(
                "\"call \"%BAM_VCVARS_BAT%\" {arg} >NUL 2>&1 && set\""
            ));
        }
        None => {
            cmd.raw_arg("\"call \"%BAM_VCVARS_BAT%\" >NUL 2>&1 && set\"");
        }
    }

    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let vars = parse_vcvars_env_output(&stdout);

    let path_val = vars
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("PATH"))
        .map(|(_, v)| v.as_str())?;
    let has_include = vars
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("INCLUDE") && !v.is_empty());
    let has_lib = vars
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("LIB") && !v.is_empty());
    if !has_include || !has_lib {
        return None;
    }

    let cl_exe = find_exe_in_path_list("cl.exe", path_val);
    Some(MsvcEnv { vars, cl_exe })
}

#[cfg(windows)]
fn cached_msvc_env() -> Option<&'static MsvcEnv> {
    static MSVC_ENV: OnceLock<Option<MsvcEnv>> = OnceLock::new();
    MSVC_ENV
        .get_or_init(|| {
            for install_root in find_vs_install_roots() {
                for (bat_path, bat_arg) in
                    msvc_vcvars_bat_candidates(&install_root, env::consts::ARCH)
                {
                    if bat_path.is_file()
                        && let Some(captured) = capture_vcvars_env(&bat_path, bat_arg)
                    {
                        return Some(captured);
                    }
                }
            }
            None
        })
        .as_ref()
}

fn msvc_command(program: &str) -> Command {
    #[cfg(windows)]
    if is_msvc_style_driver(program) {
        let has_include = env::var_os("INCLUDE").is_some_and(|v| !v.is_empty());
        let has_lib = env::var_os("LIB").is_some_and(|v| !v.is_empty());
        let explicit_path = Path::new(program).components().count() > 1;
        let direct_driver_ok = if !is_plain_cl_driver(program) {
            true
        } else if explicit_path {
            Path::new(program).is_file()
        } else {
            env::var("PATH")
                .ok()
                .and_then(|path_val| find_exe_in_path_list(program, &path_val))
                .is_some()
        };

        if !(has_include && has_lib && direct_driver_ok)
            && let Some(msvc_env) = cached_msvc_env()
        {
            let mut cmd = if is_plain_cl_driver(program) && !explicit_path {
                match &msvc_env.cl_exe {
                    Some(cl_path) => Command::new(cl_path),
                    None => Command::new(program),
                }
            } else {
                Command::new(program)
            };
            for (k, v) in &msvc_env.vars {
                cmd.env(k, v);
            }
            return cmd;
        }
    }
    Command::new(program)
}

pub(crate) fn probe_compiler_banner(program: &str) -> Option<String> {
    let output = if is_plain_cl_driver(program) {
        msvc_command(program).output().ok()?
    } else {
        msvc_command(program).arg("--version").output().ok()?
    };
    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if output.status.success() || (is_plain_cl_driver(program) && !stderr.is_empty()) {
        let first_line = if is_plain_cl_driver(program) {
            stderr
                .lines()
                .next()
                .filter(|l| !l.trim().is_empty())
                .or_else(|| stdout.lines().next().filter(|l| !l.trim().is_empty()))
        } else {
            stdout
                .lines()
                .next()
                .filter(|l| !l.trim().is_empty())
                .or_else(|| stderr.lines().next().filter(|l| !l.trim().is_empty()))
        }
        .unwrap_or("ok");
        Some(format!("{program}: {first_line}"))
    } else {
        None
    }
}

pub(crate) fn c_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cc) = env::var("CC") {
        let trimmed = cc.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
    }
    #[cfg(target_os = "windows")]
    let defaults = ["clang", "clang-cl", "cl.exe", "cl", "gcc", "cc"];
    #[cfg(not(target_os = "windows"))]
    let defaults = ["clang", "gcc", "cc"];
    for cand in defaults {
        if !out.iter().any(|existing| existing == cand) {
            out.push(cand.to_owned());
        }
    }
    out
}

pub(crate) fn cxx_candidates() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(cxx) = env::var("CXX") {
        let trimmed = cxx.trim();
        if !trimmed.is_empty() {
            out.push(trimmed.to_owned());
        }
    }
    #[cfg(target_os = "windows")]
    let defaults = ["clang++", "clang-cl", "cl.exe", "cl", "g++", "c++"];
    #[cfg(not(target_os = "windows"))]
    let defaults = ["clang++", "g++", "c++"];
    for cand in defaults {
        if !out.iter().any(|existing| existing == cand) {
            out.push(cand.to_owned());
        }
    }
    out
}

fn c_compiler() -> String {
    let candidates = c_candidates();
    for cand in &candidates {
        if probe_compiler_banner(cand).is_some() {
            return cand.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| "cc".to_owned())
}

fn cxx_compiler() -> String {
    let candidates = cxx_candidates();
    for cand in &candidates {
        if probe_compiler_banner(cand).is_some() {
            return cand.clone();
        }
    }
    candidates
        .into_iter()
        .next()
        .unwrap_or_else(|| "c++".to_owned())
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

#[allow(clippy::too_many_arguments)]
fn build_c_mod_command(
    cc: &str,
    project_dir: &Path,
    include_dir: &Path,
    build_dir: &Path,
    stem_from_mod: &str,
    src: &Path,
    out_path: &Path,
    release: bool,
) -> Command {
    let mut cmd = msvc_command(cc);
    cmd.current_dir(project_dir);
    if is_msvc_style_driver(cc) {
        let obj_path = build_dir.join(format!("{stem_from_mod}.c.obj"));
        let implib_path = build_dir.join(format!("{stem_from_mod}.c.lib"));
        cmd.args(["/nologo", "/LD", "/TC", "/std:c11", "/W4", "/WX"])
            .arg(if release { "/O2" } else { "/Od" });
        if include_dir.is_dir() {
            cmd.arg("/I").arg(include_dir);
        }
        cmd.arg(msvc_path_arg("/Fo:", &obj_path))
            .arg(msvc_path_arg("/Fe:", out_path))
            .arg(src)
            .arg("/link")
            .arg(msvc_path_arg("/IMPLIB:", &implib_path));
    } else {
        cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .arg(if release { "-O2" } else { "-O0" })
            .arg("-shared");
        if include_dir.is_dir() {
            cmd.arg(format!("-I{}", include_dir.display()));
        }
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(src).arg("-o").arg(out_path);
    }
    cmd
}

#[allow(clippy::too_many_arguments)]
fn build_cpp17_mod_command(
    cxx: &str,
    project_dir: &Path,
    include_dir: &Path,
    build_dir: &Path,
    stem_from_mod: &str,
    src: &Path,
    out_path: &Path,
    release: bool,
) -> Command {
    let mut cmd = msvc_command(cxx);
    cmd.current_dir(project_dir);
    if is_msvc_style_driver(cxx) {
        let obj_path = build_dir.join(format!("{stem_from_mod}.cpp.obj"));
        let implib_path = build_dir.join(format!("{stem_from_mod}.cpp.lib"));
        cmd.args([
            "/nologo",
            "/LD",
            "/TP",
            "/std:c++17",
            "/Zc:__cplusplus",
            "/EHsc",
            "/W4",
            "/WX",
        ])
        .arg(if release { "/O2" } else { "/Od" });
        if include_dir.is_dir() {
            cmd.arg("/I").arg(include_dir);
        }
        cmd.arg(msvc_path_arg("/Fo:", &obj_path))
            .arg(msvc_path_arg("/Fe:", out_path))
            .arg(src)
            .arg("/link")
            .arg(msvc_path_arg("/IMPLIB:", &implib_path));
    } else {
        cmd.args(["-std=c++17", "-Wall", "-Wextra", "-Wpedantic", "-Werror"])
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
        cmd.arg(src).arg("-o").arg(out_path);
    }
    cmd
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
            let mut cmd = build_c_mod_command(
                &cc,
                &project_dir,
                &include_dir,
                &build_dir,
                &stem_from_mod,
                &src,
                &out_path,
                release,
            );
            run_command(&mut cmd, &format!("{cc} C99/C11 build"))?;
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
            let mut cmd = build_cpp17_mod_command(
                &cxx,
                &project_dir,
                &include_dir,
                &build_dir,
                &stem_from_mod,
                &src,
                &out_path,
                release,
            );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn msvc_driver_classification_distinguishes_cl_clang_cl_and_gnu() {
        assert!(is_msvc_style_driver("cl"));
        assert!(is_msvc_style_driver("cl.exe"));
        assert!(is_msvc_style_driver("C:/MSVC/bin/Hostx64/x64/CL.EXE"));
        assert!(is_msvc_style_driver("clang-cl"));
        assert!(is_msvc_style_driver("clang-cl.exe"));
        assert!(!is_msvc_style_driver("clang"));
        assert!(!is_msvc_style_driver("clang++"));
        assert!(!is_msvc_style_driver("gcc"));
        assert!(!is_msvc_style_driver("x86_64-w64-mingw32-gcc"));

        assert!(is_plain_cl_driver("cl"));
        assert!(is_plain_cl_driver("cl.exe"));
        assert!(!is_plain_cl_driver("clang-cl"));
        assert!(!is_plain_cl_driver("clang"));

        let c_cands = c_candidates();
        assert!(c_cands.iter().any(|c| c == "clang"));
        assert!(c_cands.iter().any(|c| c == "gcc"));
        let cxx_cands = cxx_candidates();
        assert!(cxx_cands.iter().any(|c| c == "clang++"));
        assert!(cxx_cands.iter().any(|c| c == "g++"));
    }

    #[test]
    fn parse_vcvars_env_output_and_msvc_command_flags() {
        let sample = "\
=D:=D:\\a\\repo\r
Path=C:\\VS\\bin\\Hostx64\\x64;C:\\Windows\\System32\r
INCLUDE=C:\\VS\\include\r
LIB=C:\\VS\\lib\\x64\r
WindowsSdkDir=C:\\Windows Kits\\10\\\r
BAM_VCVARS_BAT=C:\\VS\\vcvars64.bat\r
";
        let vars = parse_vcvars_env_output(sample);
        assert!(vars.iter().any(|(k, _)| k == "PATH"));
        assert!(vars.iter().any(|(k, _)| k == "INCLUDE"));
        assert!(vars.iter().any(|(k, _)| k == "LIB"));
        assert!(vars.iter().any(|(k, _)| k == "WindowsSdkDir"));
        assert!(!vars.iter().any(|(k, _)| k == "BAM_VCVARS_BAT"));

        let bats = msvc_vcvars_bat_candidates(Path::new("C:/VS"), "x86_64");
        assert_eq!(
            bats[0].0,
            Path::new("C:/VS/VC/Auxiliary/Build/vcvars64.bat")
        );
        assert_eq!(bats[0].1, None);

        let proj = Path::new("/tmp/my mod");
        let inc = Path::new("/tmp/my mod/include");
        let bdir = Path::new("/tmp/my mod/target/release");
        let src_c = Path::new("/tmp/my mod/src/mod.c");
        let src_cpp = Path::new("/tmp/my mod/src/mod.cpp");
        let dll = Path::new("/tmp/my mod/target/release/my_mod.dll");

        let c_cmd = build_c_mod_command("cl.exe", proj, inc, bdir, "my_mod", src_c, dll, true);
        let c_args: Vec<String> = c_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(c_args.contains(&"/nologo".to_owned()));
        assert!(c_args.contains(&"/LD".to_owned()));
        assert!(c_args.contains(&"/TC".to_owned()));
        assert!(c_args.contains(&"/std:c11".to_owned()));
        assert!(c_args.contains(&"/W4".to_owned()));
        assert!(c_args.contains(&"/WX".to_owned()));
        assert!(c_args.contains(&"/O2".to_owned()));
        assert!(c_args.contains(&format!("/Fo:{}", bdir.join("my_mod.c.obj").display())));
        assert!(c_args.contains(&format!("/Fe:{}", dll.display())));
        assert!(c_args.contains(&format!("/IMPLIB:{}", bdir.join("my_mod.c.lib").display())));

        let cpp_cmd =
            build_cpp17_mod_command("cl.exe", proj, inc, bdir, "my_mod", src_cpp, dll, false);
        let cpp_args: Vec<String> = cpp_cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(cpp_args.contains(&"/LD".to_owned()));
        assert!(cpp_args.contains(&"/TP".to_owned()));
        assert!(cpp_args.contains(&"/std:c++17".to_owned()));
        assert!(cpp_args.contains(&"/Zc:__cplusplus".to_owned()));
        assert!(cpp_args.contains(&"/EHsc".to_owned()));
        assert!(cpp_args.contains(&"/W4".to_owned()));
        assert!(cpp_args.contains(&"/WX".to_owned()));
        assert!(cpp_args.contains(&"/Od".to_owned()));
        assert!(cpp_args.contains(&format!("/Fo:{}", bdir.join("my_mod.cpp.obj").display())));
        assert!(cpp_args.contains(&format!("/Fe:{}", dll.display())));
    }
}
