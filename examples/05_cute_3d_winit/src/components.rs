//! Host ECS components exposed across the zero-serialization C-ABI boundary
//! for Example 5: Cute 3D Slime Meadow (`winit` + `wgpu`).

use bevy_ecs::component::Component;
use host_runtime::ModExposable;

/// 3D world-space position of a slime spirit.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Position {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// 3D linear velocity of a slime spirit.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Velocity {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Non-uniform 3D squash-and-stretch scale of a slime spirit.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Scale3D {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Scale3D {
    pub const fn uniform(s: f32) -> Self {
        Self { x: s, y: s, z: s }
    }
}

/// Pastel RGBA color of a slime spirit's jelly body.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct ColorRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl ColorRgba {
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

/// Cute expressive animation parameters mutated by dynamic guest mods.
///
/// - `yaw`: Facing angle in radians around the Y axis.
/// - `ear_wiggle`: Ear tilt angle in radians.
/// - `blush`: Cheek rosy intensity in `[0.0, 1.0]`.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct CuteSpirit {
    pub yaw: f32,
    pub ear_wiggle: f32,
    pub blush: f32,
}

impl CuteSpirit {
    pub const fn new(yaw: f32, ear_wiggle: f32, blush: f32) -> Self {
        Self {
            yaw,
            ear_wiggle,
            blush,
        }
    }
}

/// Host-private baseline color and phase so unloading a mod smoothly restores each slime's original look.
#[derive(Component, Debug, Clone, Copy)]
pub struct HomeStyle {
    pub base_color: ColorRgba,
    pub phase_offset: f32,
}
