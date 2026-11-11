//! ECS lifecycle components, observer events, and buffered messages for native mods.

use bevy_abi_mod_host_sys::ModId;
pub use bevy_abi_mod_host_sys::{ModSystemError, ModSystemFaulted, SystemInitError};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::event::Event;
use bevy_ecs::message::Message;
use std::path::PathBuf;

/// ECS [`Component`] attached to the entity spawned for each active native mod.
#[derive(Component, Debug, Clone, PartialEq, Eq)]
pub struct LoadedModInfo {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// Human-readable mod name (file stem or path display).
    pub name: String,
    /// Filesystem path of the shared library.
    pub path: PathBuf,
    /// Number of dynamic systems registered by this mod.
    pub system_count: usize,
}

/// Bevy [`Event`] (for [`Observer`](bevy_ecs::observer::Observer)s) and buffered [`Message`]
/// emitted whenever a native mod is loaded into a Bevy [`App`](bevy_app::App).
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
pub struct ModLoaded {
    /// Assigned [`ModId`].
    pub id: ModId,
    /// ECS entity representing this loaded mod (`LoadedModInfo` + `Name`).
    pub entity: Entity,
    /// Filesystem path of the loaded library.
    pub path: PathBuf,
    /// Number of dynamic systems registered into Bevy schedules.
    pub system_count: usize,
}

/// Bevy [`Event`] (for [`Observer`](bevy_ecs::observer::Observer)s) and buffered [`Message`]
/// emitted whenever a native mod is unloaded from a Bevy [`App`](bevy_app::App).
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
pub struct ModUnloaded {
    /// Unloaded [`ModId`].
    pub id: ModId,
    /// Filesystem path of the unloaded library, if known.
    pub path: Option<PathBuf>,
    /// Number of dynamic systems excised from Bevy schedules.
    pub removed_systems: usize,
}
