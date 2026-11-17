#![forbid(unsafe_code)]
//! Shared `#[repr(C)]` `ModComponent` types and Zig build helper for Example 08 (Zig Guest Mod).

use bevy_abi_mod::prelude::*;
use bevy_abi_mod_sys::FfiBool;
use bevy_ecs::component::Component;
use bevy_ecs::resource::Resource;
use std::path::PathBuf;
use std::process::Command;

/// 3D world-space position component exposed to the Zig guest (`"example::Position"`).
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

/// 3D linear velocity component exposed to the Zig guest (`"example::Velocity"`).
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

/// Singleton simulation configuration resource exposed to the Zig guest (`"example::ZigSimConfig"`).
#[repr(C)]
#[derive(Resource, Debug, Clone, Copy, PartialEq, ModComponent)]
#[mod_component(name = "example::ZigSimConfig")]
pub struct ZigSimConfig {
    /// Timestep multiplier applied per `Update` tick.
    pub dt: f32,
    /// When `FfiBool::TRUE`, instructs the Zig guest to return `error.InvalidArgument` (`BAM_RESULT_INVALID_ARGUMENT`).
    pub trigger_error: FfiBool,
    /// Explicit C alignment padding to 8 bytes.
    pub pad: [u8; 3],
}

impl Default for ZigSimConfig {
    fn default() -> Self {
        Self {
            dt: 0.25,
            trigger_error: FfiBool::FALSE,
            pad: [0; 3],
        }
    }
}

/// Returns `true` if a `zig` compiler executable is available on `PATH`.
pub fn zig_is_available() -> bool {
    Command::new("zig")
        .arg("version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Resolves an existing pre-built Zig shared library or compiles `zig_mod/` if `zig` is installed.
///
/// Returns `None` when no pre-built library is staged and `zig` is not installed locally.
pub fn try_resolve_or_build_zig_mod() -> Option<PathBuf> {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let (prefix, ext) = if cfg!(target_os = "windows") {
        ("", "dll")
    } else if cfg!(target_os = "macos") {
        ("lib", "dylib")
    } else {
        ("lib", "so")
    };
    let filename = format!("{prefix}zig_mod.{ext}");

    if let Some(dir) = std::env::var_os("BAM_FIXTURE_DIR").filter(|v| !v.is_empty()) {
        let staged = PathBuf::from(dir).join(&filename);
        if staged.is_file() {
            return Some(staged);
        }
    }

    let zig_dir = manifest_dir.join("zig_mod");
    let candidates = [
        zig_dir.join("zig-out/lib").join(&filename),
        zig_dir.join("zig-out/bin").join(&filename),
    ];
    for candidate in &candidates {
        if candidate.is_file() {
            return Some(candidate.clone());
        }
    }

    if !zig_is_available() {
        return None;
    }

    let status = Command::new("zig")
        .current_dir(&zig_dir)
        .args(["build", "-Doptimize=ReleaseFast"])
        .status()
        .expect("Failed to execute `zig build`");
    assert!(status.success(), "`zig build` failed for Example 08");

    for candidate in &candidates {
        if candidate.is_file() {
            return Some(candidate.clone());
        }
    }
    panic!(
        "`zig build` succeeded, but expected shared library `{filename}` was not found under zig-out/"
    );
}
