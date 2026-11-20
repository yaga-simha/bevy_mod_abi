//! Host components for Example 6: Cross-Mod Dynamic Components.

use bevy_ecs::component::Component;
use bevy_mod_abi::ModComponent;

/// Host-native component representing the spatial sector of the fleet (`"example::FleetSector"`).
#[repr(C)]
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, ModComponent)]
#[mod_component(name = "example::FleetSector")]
pub struct FleetSector {
    /// Sector identifier.
    pub sector_id: u32,
    /// Relative danger scalar `[0.0, 1.0]`.
    pub danger_level: f32,
}
