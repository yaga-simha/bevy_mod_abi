//! Components for Example 3: Cross-Language Interoperability

use bevy_ecs::component::Component;
use host_runtime::ModExposable;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct ColorRgba {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}
