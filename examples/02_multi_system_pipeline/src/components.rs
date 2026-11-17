//! Host components for Example 2: Multi-System Pipeline.

use bevy_abi_mod::ModComponent;
use bevy_ecs::component::Component;

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

/// Shared hit-point pool component (`"example::Health"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::Health")]
pub struct Health {
    /// Current hit points.
    pub current: f32,
    /// Maximum hit points.
    pub max: f32,
}

/// Marker/metadata tag identifying player-controlled ships (`"example::PlayerTag"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::PlayerTag")]
pub struct PlayerTag {
    /// Player slot identifier.
    pub id: u32,
}

/// Marker/metadata tag identifying autonomous fleet drones (`"example::DroneTag"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq, ModComponent)]
#[mod_component(name = "example::DroneTag")]
pub struct DroneTag {
    /// Assigned squadron number.
    pub squad: u32,
}
