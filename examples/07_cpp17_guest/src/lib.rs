#![forbid(unsafe_code)]
//! Shared `#[repr(C)]` `ModComponent` types for Example 07 (C++17 Guest Mod).

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::{FfiBool, FfiOption, ModComponent as ModComponentTrait};
use bevy_ecs::component::Component;
use bevy_ecs::resource::Resource;
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

/// Telemetry resource updated by C++17 Wave 1–2 systems & lifecycle observers (`"example::CppTelemetry"`).
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

fn find_cxx_compiler() -> String {
    if let Ok(cxx) = std::env::var("CXX")
        && !cxx.trim().is_empty()
    {
        return cxx.trim().to_owned();
    }
    #[cfg(target_os = "windows")]
    let candidates = ["clang++", "clang-cl", "cl.exe", "cl", "g++", "c++"];
    #[cfg(not(target_os = "windows"))]
    let candidates = ["clang++", "g++", "c++"];
    for cand in candidates {
        let is_plain_cl = Path::new(cand)
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|n| n.eq_ignore_ascii_case("cl") || n.eq_ignore_ascii_case("cl.exe"));
        let res = if is_plain_cl {
            Command::new(cand).output()
        } else {
            Command::new(cand).arg("--version").output()
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
    let obj_path = std::env::temp_dir().join(format!("cpp17_mod_{pid}.obj"));
    let implib_path = std::env::temp_dir().join(format!("cpp17_mod_{pid}.lib"));
    let exp_path = std::env::temp_dir().join(format!("cpp17_mod_{pid}.exp"));

    let mut cmd = Command::new(&cxx);
    cmd.current_dir(cpp_dir);
    if is_msvc_style_driver(&cxx) {
        cmd.args([
            "/nologo",
            "/LD",
            "/std:c++17",
            "/Zc:__cplusplus",
            "/EHsc",
            "/W4",
            "/WX",
            "/O2",
        ])
        .arg(format!("/I{}", include_dir.display()))
        .arg(format!("/Fo{}", obj_path.display()))
        .arg("mod.cpp")
        .arg("/link")
        .arg(format!("/IMPLIB:{}", implib_path.display()))
        .arg(format!("/OUT:{}", tmp_path.display()));
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
