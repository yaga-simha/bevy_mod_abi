#![forbid(unsafe_code)]
//! Shared `#[repr(C)]` `ModComponent` types for Example 07 (C++17 Guest Mod).

use bevy_abi_mod_sys::{FfiBool, FfiOption, ModComponent as ModComponentTrait};
use bevy_ecs::component::Component;
use bevy_ecs::resource::Resource;
use bevy_mod_abi::prelude::*;
use std::env;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;
use std::time::SystemTime;

/// 3D world-space position component exposed to the C++17 guest (`"example::Position"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    /// X coordinate.
    pub x: f32,
    /// Y coordinate.
    pub y: f32,
    /// Z coordinate.
    pub z: f32,
}

impl Position {
    /// Creates a new [`Position`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// 3D linear velocity component exposed to the C++17 guest (`"example::Velocity"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Velocity")]
pub struct Velocity {
    /// X velocity.
    pub x: f32,
    /// Y velocity.
    pub y: f32,
    /// Z velocity.
    pub z: f32,
}

impl Velocity {
    /// Creates a new [`Velocity`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Singleton simulation configuration resource exposed to the C++17 guest (`"example::SimConfig"`).
#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "example::SimConfig")]
pub struct SimConfig {
    /// Fixed integration timestep in seconds.
    pub dt: f32,
    /// Per-tick velocity damping factor.
    pub damping: f32,
    /// When `FfiBool::TRUE`, instructs the C++17 guest to throw a `std::runtime_error` caught at the ABI boundary.
    pub trigger_exception: FfiBool,
    /// Explicit C alignment padding to 12 bytes.
    pub pad: [u8; 3],
}

impl Default for SimConfig {
    fn default() -> Self {
        Self {
            dt: 0.5,
            damping: 0.5,
            trigger_exception: FfiBool::FALSE,
            pad: [0; 3],
        }
    }
}

/// Non-POD actor stats component demonstrating `Vec<u32>`, `String`, `FfiOption<u32>`, and `bool`
/// accessed through C++17 `ActorStatsView` and `HostVecView` (`"example::ActorStats"`).
#[derive(Component, Debug, Clone, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::ActorStats")]
pub struct ActorStats {
    /// Whether the actor is active.
    pub active: bool,
    /// Host-owned actor label.
    pub tag: String,
    /// Host-owned score history vector.
    pub scores: Vec<u32>,
    /// Optional bonus points.
    pub bonus: FfiOption<u32>,
}

/// Tagged `#[repr(C, u8)]` enum component accessed through C++17 `BehaviorModeView` (`"example::BehaviorMode"`).
#[repr(C, u8)]
#[derive(Component, Debug, Clone, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::BehaviorMode")]
pub enum BehaviorMode {
    /// Idle state.
    #[default]
    Idle = 0,
    /// Patrolling with speed and aggression flag.
    Patrol {
        /// Patrol speed.
        speed: f32,
        /// Aggression flag.
        aggressive: bool,
    } = 1,
    /// Attacking target ID.
    Attack(u32) = 2,
}

/// Telemetry resource updated by C++17 systems and lifecycle observers (`"example::CppTelemetry"`).
#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::CppTelemetry")]
pub struct CppTelemetry {
    /// Number of `Add<ActorStats>` lifecycle observer triggers observed by C++17.
    pub observed_adds: u32,
    /// Number of `Position` entities where `Has<ActorStats>` was `true`.
    pub entities_with_stats: u32,
    /// Number of `Position` entities where `Has<ActorStats>` was `false`.
    pub entities_without_stats: u32,
    /// Number of entities carrying `ChildOf` seen by the C++17 query.
    pub child_entities_seen: u32,
    /// Requested number of `Position` entities to spawn via `cmd_spawn_batch`.
    pub spawn_batch_count: u32,
    /// Set to `FfiBool::TRUE` once the batch spawn has been queued.
    pub batch_spawned: FfiBool,
    /// Explicit C alignment padding.
    pub pad: [u8; 3],
}

/// Renders `examples/07_cpp17_guest/cpp_mod/example_components.h` from `ModComponent::C_DECL`.
pub fn render_example_components_header() -> String {
    let mut out = String::with_capacity(16 * 1024);
    out.push_str("/* AUTO-GENERATED FROM `ModComponent::C_DECL` — DO NOT EDIT BY HAND. */\n");
    out.push_str("#ifndef EXAMPLE_07_COMPONENTS_H\n");
    out.push_str("#define EXAMPLE_07_COMPONENTS_H\n\n");
    out.push_str("#include \"../../../include/bevy_abi_mod.h\"\n\n");
    out.push_str("#ifdef __cplusplus\nextern \"C\" {\n#endif\n\n");
    for decl in [
        <Position as ModComponentTrait>::C_DECL,
        <Velocity as ModComponentTrait>::C_DECL,
        <SimConfig as ModComponentTrait>::C_DECL,
        <ActorStats as ModComponentTrait>::C_DECL,
        <BehaviorMode as ModComponentTrait>::C_DECL,
        <CppTelemetry as ModComponentTrait>::C_DECL,
    ] {
        out.push_str(decl);
        out.push('\n');
    }
    out.push_str("#ifdef __cplusplus\n}\n#endif\n\n");
    out.push_str("#endif /* EXAMPLE_07_COMPONENTS_H */\n");
    out
}

static BUILT_CPP17_MOD: OnceLock<PathBuf> = OnceLock::new();

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn is_dylib_up_to_date(
    dylib_path: &Path,
    src_path: &Path,
    gen_header_path: &Path,
    sdk_header_path: &Path,
    abi_header_path: &Path,
) -> bool {
    let Some(dylib_mtime) = file_mtime(dylib_path) else {
        return false;
    };
    let Some(src_mtime) = file_mtime(src_path) else {
        return false;
    };
    let Some(gen_mtime) = file_mtime(gen_header_path) else {
        return false;
    };
    let Some(sdk_mtime) = file_mtime(sdk_header_path) else {
        return false;
    };
    let Some(abi_mtime) = file_mtime(abi_header_path) else {
        return false;
    };
    dylib_mtime >= src_mtime
        && dylib_mtime >= gen_mtime
        && dylib_mtime >= sdk_mtime
        && dylib_mtime >= abi_mtime
}

/// Resolves or builds the C++17 guest shared library (`libcpp17_mod.{dylib,so}` / `cpp17_mod.dll`).
pub fn resolve_or_build_cpp17_mod() -> PathBuf {
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let filename = format!("{prefix}cpp17_mod.{ext}");

    if let Some(dir) = std::env::var_os("BAM_FIXTURE_DIR").filter(|v| !v.is_empty()) {
        let staged = PathBuf::from(dir).join(&filename);
        if staged.is_file() {
            return staged;
        }
    }

    BUILT_CPP17_MOD
        .get_or_init(|| {
            let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let cpp_dir = manifest_dir.join("cpp_mod");
            let dylib_path = cpp_dir.join(&filename);
            let src_path = cpp_dir.join("mod.cpp");
            let gen_header_path = cpp_dir.join("example_components.h");
            let sdk_header_path = cpp_dir.join("bam_cpp17_sdk.hpp");
            let abi_header_path = cpp_dir.join("../../../include/bevy_abi_mod.h");

            if !is_dylib_up_to_date(
                &dylib_path,
                &src_path,
                &gen_header_path,
                &sdk_header_path,
                &abi_header_path,
            ) {
                build_cpp17_mod(&cpp_dir, &dylib_path, ext);
            }
            dylib_path
        })
        .clone()
}

fn is_msvc_style_driver(tool: &str) -> bool {
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

fn find_cxx_compiler() -> String {
    if let Ok(cxx) = env::var("CXX")
        && !cxx.trim().is_empty()
    {
        return cxx.trim().to_owned();
    }
    #[cfg(target_os = "windows")]
    let candidates = ["clang++", "clang-cl", "cl.exe", "cl", "g++", "c++"];
    #[cfg(not(target_os = "windows"))]
    let candidates = ["clang++", "g++", "c++"];
    for cand in candidates {
        let is_plain_cl = is_plain_cl_driver(cand);
        let res = if is_plain_cl {
            msvc_command(cand).output()
        } else {
            msvc_command(cand).arg("--version").output()
        };
        if let Ok(out) = res
            && (out.status.success() || (is_plain_cl && !out.stderr.is_empty()))
        {
            return cand.to_owned();
        }
    }
    if cfg!(target_os = "windows") {
        "g++".to_owned()
    } else {
        "c++".to_owned()
    }
}

fn build_cpp17_mod(cpp_dir: &Path, dylib_path: &Path, ext: &str) {
    let cxx = find_cxx_compiler();
    let include_dir = cpp_dir.join("../../../include");
    let pid = std::process::id();
    let tmp_path = cpp_dir.join(format!("cpp17_mod.tmp.{pid}.{ext}"));
    let obj_path = env::temp_dir().join(format!("cpp17_mod_{pid}.obj"));
    let implib_path = env::temp_dir().join(format!("cpp17_mod_{pid}.lib"));
    let exp_path = env::temp_dir().join(format!("cpp17_mod_{pid}.exp"));

    let mut cmd = msvc_command(&cxx);
    cmd.current_dir(cpp_dir);
    if is_msvc_style_driver(&cxx) {
        cmd.args([
            "/nologo",
            "/LD",
            "/TP",
            "/std:c++17",
            "/Zc:__cplusplus",
            "/EHsc",
            "/W4",
            "/WX",
            "/O2",
        ])
        .arg("/I")
        .arg(&include_dir)
        .arg(msvc_path_arg("/Fo:", &obj_path))
        .arg(msvc_path_arg("/Fe:", &tmp_path))
        .arg("mod.cpp")
        .arg("/link")
        .arg(msvc_path_arg("/IMPLIB:", &implib_path));
    } else {
        cmd.args([
            "-std=c++17",
            "-Wall",
            "-Wextra",
            "-Wpedantic",
            "-Werror",
            "-O3",
            "-shared",
        ])
        .arg(format!("-I{}", include_dir.display()));
        if !cfg!(target_os = "windows") {
            cmd.args([
                "-fPIC",
                "-fvisibility=hidden",
                "-fvisibility-inlines-hidden",
            ]);
        }
        cmd.arg("mod.cpp").arg("-o").arg(&tmp_path);
    }
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to invoke `{cxx}` for C++17 guest mod: {e}"));
    let _ = fs::remove_file(&obj_path);
    let _ = fs::remove_file(&implib_path);
    let _ = fs::remove_file(&exp_path);
    if !status.success() {
        let _ = fs::remove_file(&tmp_path);
        panic!("C++17 guest `{cxx}` compilation failed with status {status}");
    }

    if let Err(rename_err) = fs::rename(&tmp_path, dylib_path) {
        fs::copy(&tmp_path, dylib_path).unwrap_or_else(|copy_err| {
            let _ = fs::remove_file(&tmp_path);
            panic!(
                "Failed to move compiled C++17 mod {} -> {} (rename: {rename_err}, copy: {copy_err})",
                tmp_path.display(),
                dylib_path.display()
            );
        });
        let _ = fs::remove_file(&tmp_path);
    }

    assert!(
        dylib_path.is_file(),
        "Expected compiled C++17 mod at {}",
        dylib_path.display()
    );
}
