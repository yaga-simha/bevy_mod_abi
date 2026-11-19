#![forbid(unsafe_code)]

//! Workspace automation runner (`cargo xtask`: `lint`, `ci`, `header`, `gen-component-headers`,
//! `fixtures`, `bench`, `miri`, `hook`, `sign`).

mod component_header_gen;
mod signing_cli;

use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
#[cfg(windows)]
use std::sync::OnceLock;

#[derive(Clone, Debug)]
struct BuiltFixture {
    key: &'static str,
    language: &'static str,
    profile: &'static str,
    filename: String,
    dest_path: PathBuf,
    source_rel: String,
}

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("xtask error: {err}");
            ExitCode::FAILURE
        }
    }
}

fn repo_root() -> Result<PathBuf, String> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "failed to resolve workspace root from xtask CARGO_MANIFEST_DIR".to_owned())
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

fn is_plain_cl_driver(tool: &str) -> bool {
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

fn compiler_available(program: &str) -> bool {
    let output = if is_plain_cl_driver(program) {
        msvc_command(program).output()
    } else {
        msvc_command(program).arg("--version").output()
    };
    let Ok(out) = output else {
        return false;
    };
    out.status.success() || (is_plain_cl_driver(program) && !out.stderr.is_empty())
}

pub(crate) fn c_compiler() -> String {
    if let Ok(cc) = env::var("CC")
        && !cc.trim().is_empty()
    {
        return cc.trim().to_owned();
    }
    #[cfg(target_os = "windows")]
    let defaults = ["clang", "clang-cl", "cl.exe", "cl", "gcc", "cc"];
    #[cfg(not(target_os = "windows"))]
    let defaults = ["clang", "gcc", "cc"];
    defaults
        .iter()
        .copied()
        .find(|cand| compiler_available(cand))
        .unwrap_or(if cfg!(target_os = "windows") {
            "gcc"
        } else {
            "cc"
        })
        .to_owned()
}

pub(crate) fn cxx_compiler() -> String {
    if let Ok(cxx) = env::var("CXX")
        && !cxx.trim().is_empty()
    {
        return cxx.trim().to_owned();
    }
    #[cfg(target_os = "windows")]
    let defaults = ["clang++", "clang-cl", "cl.exe", "cl", "g++", "c++"];
    #[cfg(not(target_os = "windows"))]
    let defaults = ["clang++", "g++", "c++"];
    defaults
        .iter()
        .copied()
        .find(|cand| compiler_available(cand))
        .unwrap_or(if cfg!(target_os = "windows") {
            "g++"
        } else {
            "c++"
        })
        .to_owned()
}

pub(crate) fn c_syntax_only_cmd(
    cc: &str,
    include_dir: Option<&Path>,
    header_or_c: &Path,
) -> Command {
    let mut cmd = msvc_command(cc);
    if is_msvc_style_driver(cc) {
        cmd.args(["/nologo", "/Zs", "/TC", "/std:c11", "/W4", "/WX"]);
        if let Some(inc) = include_dir {
            cmd.arg("/I").arg(inc);
        }
    } else {
        cmd.args([
            "-std=c99",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-pedantic",
            "-fsyntax-only",
            "-x",
            "c",
        ]);
        if let Some(inc) = include_dir {
            cmd.arg(format!("-I{}", inc.display()));
        }
    }
    cmd.arg(header_or_c);
    cmd
}

#[cfg(test)]
pub(crate) fn cxx17_syntax_only_cmd(
    cxx: &str,
    include_dir: Option<&Path>,
    header_or_cpp: &Path,
) -> Command {
    let mut cmd = msvc_command(cxx);
    if is_msvc_style_driver(cxx) {
        cmd.args([
            "/nologo",
            "/Zs",
            "/TP",
            "/std:c++17",
            "/Zc:__cplusplus",
            "/EHsc",
            "/W4",
            "/WX",
        ]);
        if let Some(inc) = include_dir {
            cmd.arg("/I").arg(inc);
        }
    } else {
        cmd.args([
            "-std=c++17",
            "-Wall",
            "-Wextra",
            "-Wpedantic",
            "-Werror",
            "-fsyntax-only",
            "-x",
            "c++-header",
        ]);
        if let Some(inc) = include_dir {
            cmd.arg(format!("-I{}", inc.display()));
        }
    }
    cmd.arg(header_or_cpp);
    cmd
}

fn msvc_opt_flag(opt_flag: &str) -> &'static str {
    if opt_flag == "-O0" { "/Od" } else { "/O2" }
}

fn cargo_bin() -> String {
    env::var("CARGO")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "cargo".to_owned())
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(subcmd) = args.first().map(String::as_str) else {
        print_help();
        return Ok(());
    };

    let rest = &args[1..];
    match subcmd {
        "help" | "--help" | "-h" => {
            print_help();
            Ok(())
        }
        "fixtures" => cmd_fixtures(rest),
        "lint" => cmd_lint(rest),
        "ci" => cmd_ci(rest),
        "header" => cmd_header(rest),
        "gen-component-headers" => cmd_gen_component_headers(rest),
        "bench" => cmd_bench(rest),
        "miri" => cmd_miri(rest),
        "hook" => cmd_hook(rest),
        "sign" => signing_cli::cmd_sign(rest),
        other => Err(format!(
            "unknown subcommand `{other}` (expected `fixtures`, `lint`, `ci`, `header`, `gen-component-headers`, `bench`, `miri`, `hook`, or `sign`)"
        )),
    }
}

fn print_help() {
    println!("Usage: cargo xtask <subcommand> [options]");
    println!();
    println!("Subcommands:");
    println!(
        "  fixtures [--examples] [--benches] [--all]   Build guest mod fixtures into target/fixtures/"
    );
    println!(
        "  lint [--quick]                              Run fmt, clippy, zero-Bevy check, C99 header, bevy_lint, and Bevy idiom audits"
    );
    println!(
        "  ci                                          Run fmt, clippy, zero-Bevy check, fixtures, C99, and workspace tests"
    );
    println!(
        "  header [--check] [--test-c99]               Generate & validate include/bevy_abi_mod.h as strict ISO C99"
    );
    println!(
        "  gen-component-headers [--check]             Generate & validate include/generated/*.h from ModComponent::C_DECL"
    );
    println!(
        "  bench [--permutation] [-- <args>...]        Run Criterion/host benchmarks or full permutation matrix"
    );
    println!(
        "  miri [--all]                                Run Miri (-Zmiri-tree-borrows) on ABI and guest crates"
    );
    println!(
        "  hook <install|uninstall>                    Install or remove git pre-commit hook for automated lint checks"
    );
    println!(
        "  sign <keygen|cert|file|krl|verify> [...]  Generate Ed25519 CA/signer keys, certs, KRLs, and sign & verify mods"
    );
}

fn run_cmd(cmd: &mut Command, description: &str) -> Result<(), String> {
    println!("==> {description}");
    let status = cmd
        .status()
        .map_err(|e| format!("failed to spawn `{description}`: {e}"))?;
    if !status.success() {
        return Err(format!("`{description}` failed with status {status}"));
    }
    Ok(())
}

fn build_rust_cdylib(
    root: &Path,
    manifest_rel: &str,
    crate_stem: &str,
    release: bool,
) -> Result<PathBuf, String> {
    let manifest_path = root.join(manifest_rel);
    let mut cmd = Command::new(cargo_bin());
    cmd.current_dir(root)
        .arg("build")
        .arg("--manifest-path")
        .arg(&manifest_path);
    if release {
        cmd.arg("--release");
    }
    let profile_dir = if release { "release" } else { "debug" };
    run_cmd(
        &mut cmd,
        &format!("cargo build {manifest_rel} ({profile_dir})"),
    )?;

    let (prefix, ext) = dylib_naming();
    let crate_dir = manifest_path
        .parent()
        .ok_or_else(|| format!("invalid manifest path {manifest_rel}"))?;
    let artifact = crate_dir
        .join("target")
        .join(profile_dir)
        .join(format!("{prefix}{crate_stem}.{ext}"));
    if !artifact.is_file() {
        return Err(format!(
            "expected Rust cdylib artifact was not produced at {}",
            artifact.display()
        ));
    }
    Ok(artifact)
}

#[allow(clippy::too_many_arguments)]
fn build_c_shared_cmd(
    cc: &str,
    work_dir: &Path,
    include_dir: &Path,
    obj_path: &Path,
    implib_path: &Path,
    src_path: &Path,
    out_path: &Path,
    opt_flag: &str,
    link_math: bool,
) -> Command {
    let mut cmd = msvc_command(cc);
    cmd.current_dir(work_dir);
    if is_msvc_style_driver(cc) {
        cmd.args(["/nologo", "/LD", "/TC", "/std:c11", "/W4", "/WX"])
            .arg(msvc_opt_flag(opt_flag))
            .arg("/I")
            .arg(include_dir)
            .arg(msvc_path_arg("/Fo:", obj_path))
            .arg(msvc_path_arg("/Fe:", out_path))
            .arg(src_path)
            .arg("/link")
            .arg(msvc_path_arg("/IMPLIB:", implib_path));
    } else {
        cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror"])
            .arg(format!("-I{}", include_dir.display()))
            .arg(opt_flag)
            .arg("-shared");
        if !cfg!(target_os = "windows") {
            cmd.arg("-fPIC");
        }
        cmd.arg(src_path).arg("-o").arg(out_path);
        if link_math && !cfg!(target_os = "windows") {
            cmd.arg("-lm");
        }
    }
    cmd
}

#[allow(clippy::too_many_arguments)]
fn build_cpp17_shared_cmd(
    cxx: &str,
    work_dir: &Path,
    include_dir: &Path,
    obj_path: &Path,
    implib_path: &Path,
    src_path: &Path,
    out_path: &Path,
    opt_flag: &str,
) -> Command {
    let mut cmd = msvc_command(cxx);
    cmd.current_dir(work_dir);
    if is_msvc_style_driver(cxx) {
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
        .arg(msvc_opt_flag(opt_flag))
        .arg("/I")
        .arg(include_dir)
        .arg(msvc_path_arg("/Fo:", obj_path))
        .arg(msvc_path_arg("/Fe:", out_path))
        .arg(src_path)
        .arg("/link")
        .arg(msvc_path_arg("/IMPLIB:", implib_path));
    } else {
        cmd.args(["-std=c++17", "-Wall", "-Wextra", "-Wpedantic", "-Werror"])
            .arg(format!("-I{}", include_dir.display()))
            .arg(opt_flag)
            .arg("-shared");
        if !cfg!(target_os = "windows") {
            cmd.args([
                "-fPIC",
                "-fvisibility=hidden",
                "-fvisibility-inlines-hidden",
            ]);
        }
        cmd.arg(src_path).arg("-o").arg(out_path);
    }
    cmd
}

fn build_c_standalone_exe_cmd(
    cc: &str,
    root: &Path,
    include_dir: &Path,
    obj_path: &Path,
    exe_path: &Path,
    src_path: &Path,
) -> Command {
    let mut cmd = msvc_command(cc);
    cmd.current_dir(root);
    if is_msvc_style_driver(cc) {
        cmd.args(["/nologo", "/TC", "/std:c11", "/W4", "/WX"])
            .arg("/I")
            .arg(include_dir)
            .arg(msvc_path_arg("/Fo:", obj_path))
            .arg(msvc_path_arg("/Fe:", exe_path))
            .arg(src_path);
    } else {
        cmd.args(["-std=c99", "-Wall", "-Wextra", "-Werror", "-pedantic"])
            .arg(format!("-I{}", include_dir.display()))
            .arg(src_path)
            .arg("-o")
            .arg(exe_path);
    }
    cmd
}

fn build_c99_mod(
    root: &Path,
    dir_rel: &str,
    src_file: &str,
    artifact_stem: &str,
    opt_flag: &str,
    link_math: bool,
) -> Result<PathBuf, String> {
    let mod_dir = root.join(dir_rel);
    let (prefix, ext) = dylib_naming();
    let out_name = format!("{prefix}{artifact_stem}.{ext}");
    let out_path = mod_dir.join(&out_name);

    let cc = c_compiler();
    let include_dir = root.join("include");
    let obj_dir = root.join("target").join("c_mods_obj");
    fs::create_dir_all(&obj_dir)
        .map_err(|e| format!("failed to create {}: {e}", obj_dir.display()))?;
    let obj_path = obj_dir.join(format!("{artifact_stem}.c.obj"));
    let implib_path = obj_dir.join(format!("{artifact_stem}.c.lib"));
    let mut cmd = build_c_shared_cmd(
        &cc,
        &mod_dir,
        &include_dir,
        &obj_path,
        &implib_path,
        Path::new(src_file),
        &out_path,
        opt_flag,
        link_math,
    );
    run_cmd(
        &mut cmd,
        &format!("{cc} {dir_rel}/{src_file} -> {out_name}"),
    )?;

    if !out_path.is_file() {
        return Err(format!(
            "expected C99 shared library was not produced at {}",
            out_path.display()
        ));
    }
    Ok(out_path)
}

fn build_cpp17_mod(
    root: &Path,
    dir_rel: &str,
    src_file: &str,
    artifact_stem: &str,
    opt_flag: &str,
) -> Result<PathBuf, String> {
    let mod_dir = root.join(dir_rel);
    let (prefix, ext) = dylib_naming();
    let out_name = format!("{prefix}{artifact_stem}.{ext}");
    let out_path = mod_dir.join(&out_name);

    let cxx = cxx_compiler();
    let include_dir = root.join("include");
    let obj_dir = root.join("target").join("c_mods_obj");
    fs::create_dir_all(&obj_dir)
        .map_err(|e| format!("failed to create {}: {e}", obj_dir.display()))?;
    let obj_path = obj_dir.join(format!("{artifact_stem}.cpp.obj"));
    let implib_path = obj_dir.join(format!("{artifact_stem}.cpp.lib"));
    let mut cmd = build_cpp17_shared_cmd(
        &cxx,
        &mod_dir,
        &include_dir,
        &obj_path,
        &implib_path,
        Path::new(src_file),
        &out_path,
        opt_flag,
    );
    run_cmd(
        &mut cmd,
        &format!("{cxx} {dir_rel}/{src_file} -> {out_name}"),
    )?;

    if !out_path.is_file() {
        return Err(format!(
            "expected C++17 shared library was not produced at {}",
            out_path.display()
        ));
    }
    Ok(out_path)
}

fn build_c99_test_fixture(root: &Path, stem: &str) -> Result<PathBuf, String> {
    let src_dir = root.join("tests").join("fixtures").join("c_mods");
    let src_path = src_dir.join(format!("{stem}.c"));
    let build_dir = root.join("target").join("c_mods_build");
    fs::create_dir_all(&build_dir)
        .map_err(|e| format!("failed to create {}: {e}", build_dir.display()))?;

    let (prefix, ext) = dylib_naming();
    let out_name = format!("{prefix}{stem}.{ext}");
    let out_path = build_dir.join(&out_name);

    let cc = c_compiler();
    let include_dir = root.join("include");
    let obj_path = build_dir.join(format!("{stem}.obj"));
    let implib_path = build_dir.join(format!("{stem}.lib"));
    let mut cmd = build_c_shared_cmd(
        &cc,
        root,
        &include_dir,
        &obj_path,
        &implib_path,
        &src_path,
        &out_path,
        "-O2",
        false,
    );
    run_cmd(
        &mut cmd,
        &format!("{cc} tests/fixtures/c_mods/{stem}.c -> target/c_mods_build/{out_name}"),
    )?;

    if !out_path.is_file() {
        return Err(format!(
            "expected C99 test fixture was not produced at {}",
            out_path.display()
        ));
    }
    Ok(out_path)
}

fn stage_fixture(
    root: &Path,
    fixtures_dir: &Path,
    key: &'static str,
    language: &'static str,
    profile: &'static str,
    source_path: &Path,
    dest_stem: &str,
) -> Result<BuiltFixture, String> {
    let (prefix, ext) = dylib_naming();
    let filename = format!("{prefix}{dest_stem}.{ext}");
    let dest_path = fixtures_dir.join(&filename);
    fs::copy(source_path, &dest_path).map_err(|e| {
        format!(
            "failed to copy {} -> {}: {e}",
            source_path.display(),
            dest_path.display()
        )
    })?;

    let source_rel = source_path
        .strip_prefix(root)
        .unwrap_or(source_path)
        .to_string_lossy()
        .replace('\\', "/");

    Ok(BuiltFixture {
        key,
        language,
        profile,
        filename,
        dest_path,
        source_rel,
    })
}

fn json_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

fn write_manifest(fixtures_dir: &Path, built: &[BuiltFixture]) -> Result<PathBuf, String> {
    let (prefix, ext) = dylib_naming();
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str("  \"schema_version\": 1,\n");
    json.push_str(&format!(
        "  \"os\": \"{}\",\n",
        json_escape(env::consts::OS)
    ));
    json.push_str(&format!(
        "  \"arch\": \"{}\",\n",
        json_escape(env::consts::ARCH)
    ));
    json.push_str(&format!(
        "  \"dylib_prefix\": \"{}\",\n",
        json_escape(prefix)
    ));
    json.push_str(&format!(
        "  \"dylib_extension\": \"{}\",\n",
        json_escape(ext)
    ));
    json.push_str("  \"fixtures\": {\n");

    for (idx, item) in built.iter().enumerate() {
        let comma = if idx + 1 == built.len() { "" } else { "," };
        let abs_path = item.dest_path.to_string_lossy().replace('\\', "/");
        json.push_str(&format!("    \"{}\": {{\n", json_escape(item.key)));
        json.push_str(&format!(
            "      \"language\": \"{}\",\n",
            json_escape(item.language)
        ));
        json.push_str(&format!(
            "      \"profile\": \"{}\",\n",
            json_escape(item.profile)
        ));
        json.push_str(&format!(
            "      \"filename\": \"{}\",\n",
            json_escape(&item.filename)
        ));
        json.push_str(&format!(
            "      \"path\": \"{}\",\n",
            json_escape(&abs_path)
        ));
        json.push_str(&format!(
            "      \"source_path\": \"{}\"\n",
            json_escape(&item.source_rel)
        ));
        json.push_str(&format!("    }}{comma}\n"));
    }

    json.push_str("  }\n");
    json.push_str("}\n");

    let manifest_path = fixtures_dir.join("manifest.json");
    fs::write(&manifest_path, json)
        .map_err(|e| format!("failed to write {}: {e}", manifest_path.display()))?;
    Ok(manifest_path)
}

fn build_fixtures_internal(
    root: &Path,
    include_examples: bool,
    include_benches: bool,
) -> Result<PathBuf, String> {
    let fixtures_dir = root.join("target").join("fixtures");
    fs::create_dir_all(&fixtures_dir)
        .map_err(|e| format!("failed to create {}: {e}", fixtures_dir.display()))?;

    let mut built = Vec::new();

    // 1. Core asymmetric test fixtures + `panic = "abort"` Rust guest fixture
    let rust_mod = build_rust_cdylib(
        root,
        "tests/asymmetric_rust_mod/Cargo.toml",
        "asymmetric_rust_mod",
        true,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "asymmetric_rust_mod",
        "rust",
        "release",
        &rust_mod,
        "asymmetric_rust_mod",
    )?);

    let abort_mod = build_rust_cdylib(
        root,
        "tests/fixtures/panic_abort_mod/Cargo.toml",
        "panic_abort_mod",
        true,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "panic_abort_mod",
        "rust-abort",
        "release",
        &abort_mod,
        "panic_abort_mod",
    )?);

    let c_mod = build_c99_mod(
        root,
        "tests/asymmetric_c_mod",
        "mod.c",
        "asymmetric_c_mod",
        "-O3",
        false,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "asymmetric_c_mod",
        "c99",
        "release",
        &c_mod,
        "asymmetric_c_mod",
    )?);

    // 2. Hermetic C99 integration test fixtures (`tests/fixtures/c_mods/*.c`)
    let c_test_fixtures: &[&str] = &[
        "c_uuid_oracle",
        "vtable_eviction_mod",
        "init_rollback_mod",
        "unresolved_ordering_mod",
        "late_startup_mod",
        "mod_alpha",
        "mod_beta",
        "mod_producer",
        "mod_consumer",
        "ordered_sets_mod",
        "state_hooks_and_observers_mod",
    ];
    for &stem in c_test_fixtures {
        let compiled = build_c99_test_fixture(root, stem)?;
        let mut staged =
            stage_fixture(root, &fixtures_dir, stem, "c99", "release", &compiled, stem)?;
        staged.source_rel = format!("tests/fixtures/c_mods/{stem}.c");
        built.push(staged);
    }

    // 3. Lightweight Bevy-free example 01..04 guest mods exercised by host E2E tests
    let test_rust_examples: &[(&str, &str, &str, &str)] = &[
        (
            "example_01_guest_mod",
            "examples/01_minimal_handshake/guest_mod/Cargo.toml",
            "minimal_guest_mod",
            "minimal_guest_mod",
        ),
        (
            "example_02_guest_mod",
            "examples/02_multi_system_pipeline/guest_mod/Cargo.toml",
            "example_02_guest_mod",
            "example_02_guest_mod",
        ),
        (
            "example_03_rust_mod",
            "examples/03_cross_language_interop/rust_mod/Cargo.toml",
            "rust_mod",
            "example_03_rust_mod",
        ),
        (
            "mod_gravity",
            "examples/04_visual_game/mods/mod_gravity/Cargo.toml",
            "mod_gravity",
            "mod_gravity",
        ),
        (
            "mod_rainbow",
            "examples/04_visual_game/mods/mod_rainbow/Cargo.toml",
            "mod_rainbow",
            "mod_rainbow",
        ),
        (
            "mod_vortex",
            "examples/04_visual_game/mods/mod_vortex/Cargo.toml",
            "mod_vortex",
            "mod_vortex",
        ),
        (
            "mod_shield_emitter",
            "examples/06_cross_mod_components/mods/mod_shield_emitter/Cargo.toml",
            "mod_shield_emitter",
            "mod_shield_emitter",
        ),
        (
            "mod_overcharge_siphon",
            "examples/06_cross_mod_components/mods/mod_overcharge_siphon/Cargo.toml",
            "mod_overcharge_siphon",
            "mod_overcharge_siphon",
        ),
    ];

    for &(key, manifest_rel, crate_stem, dest_stem) in test_rust_examples {
        let src = build_rust_cdylib(root, manifest_rel, crate_stem, true)?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            key,
            "rust",
            "release",
            &src,
            dest_stem,
        )?);
    }

    let ex3_c = build_c99_mod(
        root,
        "examples/03_cross_language_interop/c_mod",
        "mod.c",
        "c_mod",
        "-O3",
        false,
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "example_03_c_mod",
        "c99",
        "release",
        &ex3_c,
        "example_03_c_mod",
    )?);

    let ex7_cpp = build_cpp17_mod(
        root,
        "examples/07_cpp17_guest/cpp_mod",
        "mod.cpp",
        "cpp17_mod",
        "-O3",
    )?;
    built.push(stage_fixture(
        root,
        &fixtures_dir,
        "cpp17_mod",
        "cpp17",
        "release",
        &ex7_cpp,
        "cpp17_mod",
    )?);

    // 4. Additional optional GUI/3D example guest mods (Example 05)
    if include_examples {
        let extra_rust_examples: &[(&str, &str, &str, &str)] = &[
            (
                "mod_boing",
                "examples/05_cute_3d_winit/mods/mod_boing/Cargo.toml",
                "mod_boing",
                "mod_boing",
            ),
            (
                "mod_carousel",
                "examples/05_cute_3d_winit/mods/mod_carousel/Cargo.toml",
                "mod_carousel",
                "mod_carousel",
            ),
        ];

        for &(key, manifest_rel, crate_stem, dest_stem) in extra_rust_examples {
            let src = build_rust_cdylib(root, manifest_rel, crate_stem, true)?;
            built.push(stage_fixture(
                root,
                &fixtures_dir,
                key,
                "rust",
                "release",
                &src,
                dest_stem,
            )?);
        }

        let ex5_c = build_c99_mod(
            root,
            "examples/05_cute_3d_winit/mods/mod_cuddle_c99",
            "mod.c",
            "mod_cuddle",
            "-O3",
            true,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "mod_cuddle",
            "c99",
            "release",
            &ex5_c,
            "mod_cuddle",
        )?);
    }

    // 3. Optional benchmark guest mods
    if include_benches {
        let bench_c_rel = build_c99_mod(
            root,
            "benches/c_abi_c99_mod",
            "mod.c",
            "c_abi_c99_mod_release",
            "-O3",
            true,
        )?;
        let (prefix, ext) = dylib_naming();
        let bench_c_dbg_path = root
            .join("benches/c_abi_c99_mod")
            .join(format!("{prefix}c_abi_c99_mod_debug.{ext}"));
        let bench_c_dbg = if bench_c_dbg_path.is_file() {
            bench_c_dbg_path
        } else {
            build_c99_mod(
                root,
                "benches/c_abi_c99_mod",
                "mod.c",
                "c_abi_c99_mod_debug",
                "-O0",
                true,
            )?
        };
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_c99_mod_debug",
            "c99",
            "debug",
            &bench_c_dbg,
            "c_abi_c99_mod_debug",
        )?);
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_c99_mod_release",
            "c99",
            "release",
            &bench_c_rel,
            "c_abi_c99_mod_release",
        )?);

        let bench_rust_dbg = build_rust_cdylib(
            root,
            "benches/c_abi_rust_mod/Cargo.toml",
            "c_abi_rust_mod",
            false,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_rust_mod_debug",
            "rust",
            "debug",
            &bench_rust_dbg,
            "c_abi_rust_mod_debug",
        )?);

        let bench_rust_rel = build_rust_cdylib(
            root,
            "benches/c_abi_rust_mod/Cargo.toml",
            "c_abi_rust_mod",
            true,
        )?;
        built.push(stage_fixture(
            root,
            &fixtures_dir,
            "c_abi_rust_mod_release",
            "rust",
            "release",
            &bench_rust_rel,
            "c_abi_rust_mod_release",
        )?);
    }

    let manifest_path = write_manifest(&fixtures_dir, &built)?;
    println!(
        "==> Staged {} fixture(s) in {} (manifest: {})",
        built.len(),
        fixtures_dir.display(),
        manifest_path.display()
    );
    Ok(fixtures_dir)
}

fn cmd_fixtures(args: &[String]) -> Result<(), String> {
    let mut include_examples = false;
    let mut include_benches = false;

    for arg in args {
        match arg.as_str() {
            "--all" => {
                include_examples = true;
                include_benches = true;
            }
            "--examples" => include_examples = true,
            "--benches" => include_benches = true,
            other => {
                return Err(format!(
                    "unsupported `fixtures` flag `{other}` (expected `--all`, `--examples`, or `--benches`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    build_fixtures_internal(&root, include_examples, include_benches)?;
    Ok(())
}

fn verify_zero_bevy_deps(root: &Path) -> Result<(), String> {
    println!("==> Checking zero Bevy dependencies in ABI, guest, macro, and CLI crates");
    let output = Command::new(cargo_bin())
        .current_dir(root)
        .args([
            "tree",
            "-p",
            "bevy_abi_mod_sys",
            "-p",
            "bevy_abi_mod_guest_sys",
            "-p",
            "bevy_abi_mod_guest",
            "-p",
            "bevy_abi_mod_macros",
            "-p",
            "bevy_abi_mod_trust",
            "-p",
            "bevy_abi_mod_manifest",
            "-p",
            "bevy_abi_mod_sdk_gen",
            "-p",
            "cargo-bam",
        ])
        .output()
        .map_err(|e| format!("failed to run `cargo tree`: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "`cargo tree` failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let leaked = find_leaked_bevy_lines(&stdout);

    if !leaked.is_empty() {
        return Err(format!(
            "Bevy dependency leaked into Bevy-free crates:\n{}",
            leaked.join("\n")
        ));
    }
    Ok(())
}

/// Filters `cargo tree` output lines for unexpected `bevy` crate names while ignoring
/// filesystem checkout path suffixes like `(/home/runner/work/bevy_mod_abi/...)`.
fn find_leaked_bevy_lines(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter(|line| {
            let crate_part = line.split(" (").next().unwrap_or(line);
            !crate_part.contains("bevy_abi_mod") && crate_part.to_ascii_lowercase().contains("bevy")
        })
        .collect()
}

fn verify_c99_header_and_tests(root: &Path, run_standalone_test: bool) -> Result<(), String> {
    let header_path = root.join("include").join("bevy_abi_mod.h");
    let meta = fs::metadata(&header_path)
        .map_err(|e| format!("missing C header {}: {e}", header_path.display()))?;
    if meta.len() == 0 {
        return Err(format!("C header {} is empty", header_path.display()));
    }

    let cc = c_compiler();
    let mut syntax_cmd = c_syntax_only_cmd(&cc, None, &header_path);
    syntax_cmd.current_dir(root);
    run_cmd(
        &mut syntax_cmd,
        &format!("{cc} C99/C11 syntax check include/bevy_abi_mod.h"),
    )?;

    if run_standalone_test {
        let target_dir = root.join("target");
        fs::create_dir_all(&target_dir)
            .map_err(|e| format!("failed to create {}: {e}", target_dir.display()))?;
        let exe_name = if cfg!(target_os = "windows") {
            "standalone_c_test.exe"
        } else {
            "standalone_c_test"
        };
        let exe_path = target_dir.join(exe_name);
        let include_dir = root.join("include");
        let obj_path = target_dir.join("standalone_c_test.obj");
        let src_path = root.join("tests").join("standalone_c_test.c");
        let mut build_test =
            build_c_standalone_exe_cmd(&cc, root, &include_dir, &obj_path, &exe_path, &src_path);
        run_cmd(
            &mut build_test,
            &format!("{cc} tests/standalone_c_test.c -> target/{exe_name}"),
        )?;

        let mut exec_test = Command::new(&exe_path);
        exec_test.current_dir(root);
        run_cmd(&mut exec_test, "target/standalone_c_test")?;
    }

    Ok(())
}

fn cmd_header(args: &[String]) -> Result<(), String> {
    let mut run_standalone = false;
    let mut check_only = false;
    for arg in args {
        match arg.as_str() {
            "--test-c99" => run_standalone = true,
            "--check" => check_only = true,
            other => {
                return Err(format!(
                    "unsupported `header` flag `{other}` (expected `--check` or `--test-c99`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    bevy_abi_mod_sdk_gen::write_or_check_header(&root, check_only)?;
    verify_c99_header_and_tests(&root, run_standalone)
}

fn cmd_gen_component_headers(args: &[String]) -> Result<(), String> {
    let mut check_only = false;
    for arg in args {
        match arg.as_str() {
            "--check" => check_only = true,
            other => {
                return Err(format!(
                    "unsupported `gen-component-headers` flag `{other}` (expected `--check`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    component_header_gen::write_or_check_component_headers(&root, check_only)?;

    let cc = c_compiler();
    let gen_header = root
        .join("include")
        .join("generated")
        .join("testbed_components.h");
    let mut syntax_cmd = c_syntax_only_cmd(&cc, None, &gen_header);
    syntax_cmd.current_dir(&root);
    run_cmd(
        &mut syntax_cmd,
        &format!("{cc} C99/C11 syntax check include/generated/testbed_components.h"),
    )
}

fn run_sig_verification_clippy(root: &Path) -> Result<(), String> {
    let mut sig_clippy_cmd = Command::new(cargo_bin());
    sig_clippy_cmd.current_dir(root).args([
        "clippy",
        "-p",
        "bevy_abi_mod_host_sys",
        "-p",
        "bevy_abi_mod_host",
        "-p",
        "bevy_abi_mod",
        "--features",
        "signature-verification",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run_cmd(
        &mut sig_clippy_cmd,
        "cargo clippy --features signature-verification --all-targets -- -D warnings",
    )
}

fn cmd_lint(args: &[String]) -> Result<(), String> {
    let mut quick = false;
    for arg in args {
        match arg.as_str() {
            "--quick" => quick = true,
            other => {
                return Err(format!(
                    "unsupported `lint` flag `{other}` (expected `--quick`)"
                ));
            }
        }
    }

    let root = repo_root()?;

    let mut fmt_cmd = Command::new(cargo_bin());
    fmt_cmd
        .current_dir(&root)
        .args(["fmt", "--all", "--", "--check"]);
    run_cmd(&mut fmt_cmd, "cargo fmt --all -- --check")?;

    let mut clippy_cmd = Command::new(cargo_bin());
    clippy_cmd.current_dir(&root).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run_cmd(
        &mut clippy_cmd,
        "cargo clippy --workspace --all-targets -- -D warnings",
    )?;

    run_sig_verification_clippy(&root)?;

    verify_zero_bevy_deps(&root)?;
    verify_forbid_unsafe(&root)?;
    bevy_abi_mod_sdk_gen::write_or_check_header(&root, true)?;
    component_header_gen::write_or_check_component_headers(&root, true)?;
    verify_c99_header_and_tests(&root, !quick)?;
    check_or_run_bevy_lint(&root)?;
    audit_bevy_idioms(&root)?;

    if !quick {
        println!("==> All idiomatic Bevy and workspace lint checks passed");
    }
    Ok(())
}

fn cmd_ci(args: &[String]) -> Result<(), String> {
    if !args.is_empty() {
        return Err(format!("unexpected arguments for `ci`: {}", args.join(" ")));
    }
    let root = repo_root()?;

    let mut fmt_cmd = Command::new(cargo_bin());
    fmt_cmd
        .current_dir(&root)
        .args(["fmt", "--all", "--", "--check"]);
    run_cmd(&mut fmt_cmd, "cargo fmt --all -- --check")?;

    let mut clippy_cmd = Command::new(cargo_bin());
    clippy_cmd.current_dir(&root).args([
        "clippy",
        "--workspace",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ]);
    run_cmd(
        &mut clippy_cmd,
        "cargo clippy --workspace --all-targets -- -D warnings",
    )?;

    run_sig_verification_clippy(&root)?;

    verify_zero_bevy_deps(&root)?;

    let fixtures_dir = build_fixtures_internal(&root, false, false)?;
    bevy_abi_mod_sdk_gen::write_or_check_header(&root, true)?;
    component_header_gen::write_or_check_component_headers(&root, true)?;
    verify_c99_header_and_tests(&root, true)?;

    let mut test_cmd = Command::new(cargo_bin());
    test_cmd
        .current_dir(&root)
        .env("BAM_FIXTURE_DIR", &fixtures_dir)
        .args(["test"]);
    run_cmd(&mut test_cmd, "cargo test")?;

    let mut sig_test_cmd = Command::new(cargo_bin());
    sig_test_cmd
        .current_dir(&root)
        .env("BAM_FIXTURE_DIR", &fixtures_dir)
        .args([
            "test",
            "-p",
            "bevy_abi_mod_host_sys",
            "-p",
            "bevy_abi_mod_host",
            "-p",
            "bevy_abi_mod",
            "--features",
            "signature-verification",
        ]);
    run_cmd(
        &mut sig_test_cmd,
        "cargo test --features signature-verification",
    )?;

    Ok(())
}

fn cmd_bench(args: &[String]) -> Result<(), String> {
    let root = repo_root()?;
    let mut permutation = false;
    let mut passthrough = Vec::new();
    let mut after_sep = false;

    for arg in args {
        if after_sep {
            passthrough.push(arg.clone());
            continue;
        }
        match arg.as_str() {
            "--" => after_sep = true,
            "--permutation" => permutation = true,
            other => passthrough.push(other.to_owned()),
        }
    }

    let fixtures_dir = build_fixtures_internal(&root, false, permutation)?;

    if permutation {
        let mut cmd = Command::new(cargo_bin());
        cmd.current_dir(&root)
            .env("BAM_FIXTURE_DIR", &fixtures_dir)
            .args(["run", "--release", "-p", "permutation_bench"]);
        if !passthrough.is_empty() {
            cmd.arg("--").args(&passthrough);
        }
        run_cmd(&mut cmd, "cargo run --release -p permutation_bench")
    } else {
        let mut cmd = Command::new(cargo_bin());
        cmd.current_dir(&root)
            .env("BAM_FIXTURE_DIR", &fixtures_dir)
            .args(["bench", "-p", "bevy_abi_mod_host"]);
        if !passthrough.is_empty() {
            cmd.arg("--").args(&passthrough);
        }
        run_cmd(&mut cmd, "cargo bench -p bevy_abi_mod_host")
    }
}

fn cmd_miri(args: &[String]) -> Result<(), String> {
    let mut run_all = false;
    for arg in args {
        match arg.as_str() {
            "--all" => run_all = true,
            other => {
                return Err(format!(
                    "unsupported `miri` flag `{other}` (expected `--all`)"
                ));
            }
        }
    }

    let root = repo_root()?;
    let tree_borrows_flags = match env::var("MIRIFLAGS") {
        Ok(existing) if !existing.trim().is_empty() => format!("{existing} -Zmiri-tree-borrows"),
        _ => "-Zmiri-tree-borrows".to_owned(),
    };
    let strict_prov_flags = if tree_borrows_flags.contains("-Zmiri-strict-provenance") {
        tree_borrows_flags.clone()
    } else {
        format!("{tree_borrows_flags} -Zmiri-strict-provenance")
    };

    let mut base_cmd = Command::new("rustup");
    base_cmd
        .current_dir(&root)
        .env_remove("CARGO")
        .env_remove("RUSTC")
        .env("MIRIFLAGS", &strict_prov_flags)
        .args([
            "run",
            "nightly",
            "cargo",
            "miri",
            "test",
            "-p",
            "bevy_abi_mod_sys",
            "-p",
            "bevy_abi_mod_guest_sys",
            "-p",
            "bevy_abi_mod_guest",
        ]);
    run_cmd(
        &mut base_cmd,
        "rustup run nightly cargo miri test (-Zmiri-tree-borrows -Zmiri-strict-provenance) -p bevy_abi_mod_sys -p bevy_abi_mod_guest_sys -p bevy_abi_mod_guest",
    )?;

    if run_all {
        let mut macros_cmd = Command::new("rustup");
        macros_cmd
            .current_dir(&root)
            .env_remove("CARGO")
            .env_remove("RUSTC")
            .env("MIRIFLAGS", &tree_borrows_flags)
            .args([
                "run",
                "nightly",
                "cargo",
                "miri",
                "test",
                "-p",
                "bevy_abi_mod_macros",
                "--test",
                "mod_component_vtable",
            ]);
        run_cmd(
            &mut macros_cmd,
            "rustup run nightly cargo miri test -p bevy_abi_mod_macros --test mod_component_vtable",
        )?;

        let mut host_sys_cmd = Command::new("rustup");
        host_sys_cmd
            .current_dir(&root)
            .env_remove("CARGO")
            .env_remove("RUSTC")
            .env("MIRIFLAGS", &tree_borrows_flags)
            .args([
                "run",
                "nightly",
                "cargo",
                "miri",
                "test",
                "-p",
                "bevy_abi_mod_host_sys",
                "--lib",
            ]);
        run_cmd(
            &mut host_sys_cmd,
            "rustup run nightly cargo miri test -p bevy_abi_mod_host_sys --lib",
        )?;
    }

    Ok(())
}

fn verify_forbid_unsafe(root: &Path) -> Result<(), String> {
    println!(
        "==> Verifying #![forbid(unsafe_code)] in safe host, guest, macro, CLI, and xtask crates"
    );
    let safe_crates = [
        "crates/bevy_abi_mod/src/lib.rs",
        "crates/bevy_abi_mod_host/src/lib.rs",
        "crates/bevy_abi_mod_guest/src/lib.rs",
        "crates/bevy_abi_mod_macros/src/lib.rs",
        "crates/bevy_abi_mod_trust/src/lib.rs",
        "crates/bevy_abi_mod_manifest/src/lib.rs",
        "crates/bevy_abi_mod_sdk_gen/src/lib.rs",
        "crates/cargo-bam/src/lib.rs",
        "crates/cargo-bam/src/main.rs",
        "xtask/src/main.rs",
    ];

    for rel_path in &safe_crates {
        let full_path = root.join(rel_path);
        let content = fs::read_to_string(&full_path)
            .map_err(|e| format!("failed to read {}: {e}", full_path.display()))?;
        if !content.contains("#![forbid(unsafe_code)]") {
            return Err(format!(
                "{} is missing required `#![forbid(unsafe_code)]` declaration",
                full_path.display()
            ));
        }
    }
    Ok(())
}

fn check_or_run_bevy_lint(root: &Path) -> Result<(), String> {
    println!("==> Checking for specialized `bevy_lint`...");
    let which_cmd = if cfg!(target_os = "windows") {
        "where"
    } else {
        "which"
    };
    let has_bevy_lint = Command::new(which_cmd)
        .arg("bevy_lint")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let has_bevy_cli = Command::new(which_cmd)
        .arg("bevy")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_bevy_lint {
        println!("==> Executing `bevy_lint` on host crates...");
        let mut cmd = Command::new("bevy_lint");
        cmd.current_dir(root).args(["-p", "bevy_abi_mod_host"]);
        let _ = run_cmd(&mut cmd, "bevy_lint -p bevy_abi_mod_host");
    } else if has_bevy_cli {
        println!("==> Executing `bevy lint` on host crates...");
        let mut cmd = Command::new("bevy");
        cmd.current_dir(root)
            .args(["lint", "--", "-p", "bevy_abi_mod_host"]);
        let _ = run_cmd(&mut cmd, "bevy lint -- -p bevy_abi_mod_host");
    } else {
        println!("==> [Notice] Specialized `bevy_lint` tool not found on PATH.");
        println!("    To install the official Bevy linter (The Bevy Flock):");
        println!("      cargo install --git https://github.com/TheBevyFlock/bevy_cli bevy_cli");
        println!("      bevy lint install");
    }
    Ok(())
}

fn audit_bevy_idioms(root: &Path) -> Result<(), String> {
    println!("==> Performing static idiomatic Bevy & Rust audit...");
    let host_src = root.join("crates").join("bevy_abi_mod_host").join("src");
    if host_src.is_dir() {
        audit_dir_bevy_idioms(&host_src)?;
    }
    Ok(())
}

fn audit_dir_bevy_idioms(dir: &Path) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|e| format!("failed to read {}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            audit_dir_bevy_idioms(&path)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let content = fs::read_to_string(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
            for (line_idx, line) in content.lines().enumerate() {
                let trimmed = line.trim();
                if trimmed.contains("&mut ResMut<") {
                    eprintln!(
                        "  [anti-pattern] {}:{}: `&mut ResMut<T>` is redundant and non-idiomatic. Use `mut res: ResMut<T>`.",
                        path.display(),
                        line_idx + 1
                    );
                }
                if (trimmed.contains(".after(") || trimmed.contains(".before("))
                    && !trimmed.contains("Set")
                    && !trimmed.starts_with("//")
                {
                    eprintln!(
                        "  [idiom critique] {}:{}: direct system chaining with `{}` detected. Consider grouping systems into a `SystemSet` instead.",
                        path.display(),
                        line_idx + 1,
                        trimmed
                    );
                }
            }
        }
    }
    Ok(())
}

fn cmd_hook(args: &[String]) -> Result<(), String> {
    let root = repo_root()?;
    let action = args.first().map(String::as_str).unwrap_or("help");
    let hook_path = root.join(".git").join("hooks").join("pre-commit");

    match action {
        "install" => {
            let hook_script = "#!/bin/sh\n# Generated by `cargo xtask hook install`\nset -e\necho \"==> [pre-commit] Running idiomatic Bevy & Rust checks...\"\ncargo xtask lint --quick\n";
            fs::write(&hook_path, hook_script)
                .map_err(|e| format!("failed to write {}: {e}", hook_path.display()))?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let perms = fs::Permissions::from_mode(0o755);
                fs::set_permissions(&hook_path, perms).map_err(|e| {
                    format!(
                        "failed to set executable permissions on {}: {e}",
                        hook_path.display()
                    )
                })?;
            }

            println!(
                "==> Pre-commit hook installed successfully at {}",
                hook_path.display()
            );
            Ok(())
        }
        "uninstall" => {
            if hook_path.exists() {
                fs::remove_file(&hook_path)
                    .map_err(|e| format!("failed to remove {}: {e}", hook_path.display()))?;
                println!(
                    "==> Pre-commit hook uninstalled from {}",
                    hook_path.display()
                );
            } else {
                println!("==> No pre-commit hook found at {}", hook_path.display());
            }
            Ok(())
        }
        _ => Err("usage: cargo xtask hook <install|uninstall>".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_c_shared_cmd, build_c_standalone_exe_cmd, build_cpp17_shared_cmd, c_syntax_only_cmd,
        cxx17_syntax_only_cmd, find_leaked_bevy_lines, msvc_vcvars_bat_candidates,
        parse_vcvars_env_output,
    };
    use std::path::Path;

    #[test]
    fn find_leaked_bevy_lines_ignores_repo_path_and_catches_real_bevy_deps() {
        let sample = "\
bevy_abi_mod_sys v0.2.0 (/home/runner/work/bevy_mod_abi/bevy_mod_abi/crates/bevy_abi_mod_sys)
cargo-bam v0.2.0 (/Users/runner/work/bevy_mod_abi/bevy_mod_abi/crates/cargo-bam)
cargo-bam v0.2.0 (D:\\a\\bevy_mod_abi\\bevy_mod_abi\\crates\\cargo-bam)
├── bevy_ecs v0.20.1
└── syn v2.0.119
";
        let leaked = find_leaked_bevy_lines(sample);
        assert_eq!(leaked, vec!["├── bevy_ecs v0.20.1"]);
    }

    #[test]
    fn parse_vcvars_env_output_extracts_msvc_vars_and_skips_noise() {
        let sample = "\
=C:=C:\\work\\repo\r
BAM_VCVARS_BAT=C:\\VS\\vcvars64.bat\r
Path=C:\\VS\\VC\\Tools\\MSVC\\14.42\\bin\\Hostx64\\x64;C:\\Windows\\System32\r
INCLUDE=C:\\VS\\VC\\Tools\\MSVC\\14.42\\include;C:\\WinSDK\\ucrt\r
LIB=C:\\VS\\VC\\Tools\\MSVC\\14.42\\lib\\x64;C:\\WinSDK\\um\\x64\r
LIBPATH=C:\\VS\\VC\\Tools\\MSVC\\14.42\\lib\\x64\r
VCToolsInstallDir=C:\\VS\\VC\\Tools\\MSVC\\14.42\\\r
WindowsSdkDir=C:\\Program Files (x86)\\Windows Kits\\10\\\r
PROMPT=$P$G\r
VSCODE_IPC_HOOK=ignored\r
";
        let vars = parse_vcvars_env_output(sample);
        let get = |name: &str| {
            vars.iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            get("PATH"),
            Some("C:\\VS\\VC\\Tools\\MSVC\\14.42\\bin\\Hostx64\\x64;C:\\Windows\\System32")
        );
        assert_eq!(
            get("INCLUDE"),
            Some("C:\\VS\\VC\\Tools\\MSVC\\14.42\\include;C:\\WinSDK\\ucrt")
        );
        assert_eq!(
            get("LIB"),
            Some("C:\\VS\\VC\\Tools\\MSVC\\14.42\\lib\\x64;C:\\WinSDK\\um\\x64")
        );
        assert_eq!(
            get("LIBPATH"),
            Some("C:\\VS\\VC\\Tools\\MSVC\\14.42\\lib\\x64")
        );
        assert!(get("VCToolsInstallDir").is_some());
        assert!(get("WindowsSdkDir").is_some());
        assert!(get("PROMPT").is_none());
        assert!(get("BAM_VCVARS_BAT").is_none());
        assert!(get("VSCODE_IPC_HOOK").is_none());
    }

    #[test]
    fn msvc_vcvars_candidates_prioritize_vcvars64_on_x86_64() {
        let root = Path::new("C:/VS2022");
        let x64 = msvc_vcvars_bat_candidates(root, "x86_64");
        assert_eq!(x64[0].0, root.join("VC/Auxiliary/Build/vcvars64.bat"));
        assert_eq!(x64[0].1, None);
        assert_eq!(x64[1].0, root.join("VC/Auxiliary/Build/vcvarsall.bat"));
        assert_eq!(x64[1].1, Some("x64"));

        let arm64 = msvc_vcvars_bat_candidates(root, "aarch64");
        assert_eq!(arm64[0].0, root.join("VC/Auxiliary/Build/vcvarsall.bat"));
        assert_eq!(arm64[0].1, Some("arm64"));
    }

    #[test]
    fn msvc_command_builders_emit_strict_flags_and_colon_output_paths() {
        let work = Path::new("/tmp/my repo/mod");
        let inc = Path::new("/tmp/my repo/include");
        let obj = Path::new("/tmp/my repo/target/mod.c.obj");
        let lib = Path::new("/tmp/my repo/target/mod.c.lib");
        let dll = Path::new("/tmp/my repo/mod/mod.dll");
        let exe = Path::new("/tmp/my repo/target/standalone_c_test.exe");
        let src_c = Path::new("mod.c");
        let src_cpp = Path::new("mod.cpp");

        let c_dll = build_c_shared_cmd("cl.exe", work, inc, obj, lib, src_c, dll, "-O3", false);
        let c_args: Vec<String> = c_dll
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
        assert!(c_args.contains(&format!("/Fo:{}", obj.display())));
        assert!(c_args.contains(&format!("/Fe:{}", dll.display())));
        assert!(c_args.contains(&format!("/IMPLIB:{}", lib.display())));

        let cpp_dll = build_cpp17_shared_cmd("cl.exe", work, inc, obj, lib, src_cpp, dll, "-O0");
        let cpp_args: Vec<String> = cpp_dll
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
        assert!(cpp_args.contains(&format!("/Fo:{}", obj.display())));
        assert!(cpp_args.contains(&format!("/Fe:{}", dll.display())));

        let c_exe = build_c_standalone_exe_cmd("cl.exe", work, inc, obj, exe, src_c);
        let exe_args: Vec<String> = c_exe
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(exe_args.contains(&"/TC".to_owned()));
        assert!(exe_args.contains(&"/std:c11".to_owned()));
        assert!(exe_args.contains(&format!("/Fo:{}", obj.display())));
        assert!(exe_args.contains(&format!("/Fe:{}", exe.display())));

        let syn_c = c_syntax_only_cmd("clang-cl.exe", Some(inc), Path::new("bevy_abi_mod.h"));
        let syn_c_args: Vec<String> = syn_c
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(syn_c_args.contains(&"/Zs".to_owned()));
        assert!(syn_c_args.contains(&"/TC".to_owned()));
        assert!(syn_c_args.contains(&"/std:c11".to_owned()));
        assert!(syn_c_args.contains(&"/I".to_owned()));

        let syn_cpp = cxx17_syntax_only_cmd("cl.exe", Some(inc), Path::new("bevy_abi_mod.hpp"));
        let syn_cpp_args: Vec<String> = syn_cpp
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(syn_cpp_args.contains(&"/Zs".to_owned()));
        assert!(syn_cpp_args.contains(&"/TP".to_owned()));
        assert!(syn_cpp_args.contains(&"/std:c++17".to_owned()));
        assert!(syn_cpp_args.contains(&"/EHsc".to_owned()));
    }
}
