//! Component definitions for Example 04: Visual Kinetic Arena.
//!
//! Exposes `Position`, `Velocity`, and `ColorRgba` to guest mods via `#[derive(ModComponent)]`.

use bevy_ecs::component::Component;
use bevy_mod_abi::ModComponent;

/// 3D world-space particle position (`example::Position`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "example::Position")]
pub struct Position {
    /// Horizontal coordinate.
    pub x: f32,
    /// Vertical coordinate.
    pub y: f32,
    /// Depth coordinate.
    pub z: f32,
}

impl Position {
    /// Creates a new [`Position`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// 3D linear particle velocity (`example::Velocity`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "example::Velocity")]
pub struct Velocity {
    /// Horizontal velocity.
    pub x: f32,
    /// Vertical velocity.
    pub y: f32,
    /// Depth velocity.
    pub z: f32,
}

impl Velocity {
    /// Creates a new [`Velocity`].
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }
}

/// Linear RGBA particle color (`example::ColorRgba`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, PartialEq, Default, ModComponent)]
#[mod_component(name = "example::ColorRgba")]
pub struct ColorRgba {
    /// Red channel in `[0.0, 1.0]`.
    pub r: f32,
    /// Green channel in `[0.0, 1.0]`.
    pub g: f32,
    /// Blue channel in `[0.0, 1.0]`.
    pub b: f32,
    /// Alpha channel in `[0.0, 1.0]`.
    pub a: f32,
}

impl ColorRgba {
    /// Creates a new [`ColorRgba`].
    #[allow(dead_code)]
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    /// Baseline cyan particle tint.
    pub const fn cyan() -> Self {
        Self {
            r: 0.2,
            g: 0.8,
            b: 1.0,
            a: 1.0,
        }
    }

    /// Baseline white particle tint.
    pub const fn white() -> Self {
        Self {
            r: 1.0,
            g: 1.0,
            b: 1.0,
            a: 1.0,
        }
    }
}
