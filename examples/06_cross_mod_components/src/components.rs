//! Host components for Example 6.

use bevy_ecs::component::Component;
use host_runtime::ModExposable;

/// Optional host-native component representing the spatial sector of the fleet.
#[derive(Component, Debug, Clone, Copy, PartialEq, ModExposable)]
#[repr(C)]
pub struct FleetSector {
    pub sector_id: u32,
    pub danger_level: f32,
}
