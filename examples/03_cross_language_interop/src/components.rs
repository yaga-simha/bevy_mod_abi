//! Host components for Example 3: Cross-Language Interoperability.

use bevy_ecs::component::Component;
use bevy_mod_abi::ModComponent;

/// Shared 3D position component (`"example::Position"`).
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

/// Shared 3D linear velocity component (`"example::Velocity"`).
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

/// Shared linear RGBA color component (`"example::ColorRgba"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::ColorRgba")]
pub struct ColorRgba {
    /// Red channel `[0.0, 1.0]`.
    pub r: f32,
    /// Green channel `[0.0, 1.0]`.
    pub g: f32,
    /// Blue channel `[0.0, 1.0]`.
    pub b: f32,
    /// Alpha channel `[0.0, 1.0]`.
    pub a: f32,
}
