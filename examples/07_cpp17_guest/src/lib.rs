#![forbid(unsafe_code)]
//! Shared `#[repr(C)]` `ModComponent` types for Example 07 (C++17 Guest Mod).

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::FfiBool;
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

static BUILT_CPP17_MOD: OnceLock<PathBuf> = OnceLock::new();

fn file_mtime(path: &Path) -> Option<SystemTime> {
    fs::metadata(path).ok()?.modified().ok()
}

fn is_dylib_up_to_date(dylib_path: &Path, src_path: &Path, header_path: &Path) -> bool {
    let Some(dylib_mtime) = file_mtime(dylib_path) else {
        return false;
    };
    let Some(src_mtime) = file_mtime(src_path) else {
        return false;
    };
    let Some(hdr_mtime) = file_mtime(header_path) else {
        return false;
    };
    dylib_mtime >= src_mtime && dylib_mtime >= hdr_mtime
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
            let header_path = cpp_dir.join("../../../include/bevy_abi_mod.h");

            if !is_dylib_up_to_date(&dylib_path, &src_path, &header_path) {
                build_cpp17_mod(&cpp_dir, &dylib_path, ext);
            }
            dylib_path
        })
        .clone()
}

fn build_cpp17_mod(cpp_dir: &Path, dylib_path: &Path, ext: &str) {
    let cxx = std::env::var("CXX")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| {
            if cfg!(target_os = "windows") {
                "g++".to_owned()
            } else {
                "c++".to_owned()
            }
        });
    let include_dir = cpp_dir.join("../../../include");
    let pid = std::process::id();
    let tmp_path = cpp_dir.join(format!("cpp17_mod.tmp.{pid}.{ext}"));

    let mut cmd = Command::new(&cxx);
    cmd.current_dir(cpp_dir)
        .args([
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
    let status = cmd
        .status()
        .unwrap_or_else(|e| panic!("Failed to invoke `{cxx}` for C++17 guest mod: {e}"));
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
