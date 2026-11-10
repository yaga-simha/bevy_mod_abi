//! Components for Example 2: Multi-System Pipeline

use bevy_ecs::component::Component;
use host_runtime::ModExposable;

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Velocity {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct Health {
    pub current: f32,
    pub max: f32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct PlayerTag {
    pub id: u32,
}

#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
pub struct DroneTag {
    pub squad: u32,
}
