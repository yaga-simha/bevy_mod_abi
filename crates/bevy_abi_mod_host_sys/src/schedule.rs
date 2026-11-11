//! Schedule discriminants, system sets, and startup tracking for dynamic guest systems.

use crate::loader::ModId;
use bevy_abi_mod_sys::ScheduleLabelId;
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{Schedule, SystemSet};
use bevy_ecs::world::World;

/// Bevy [`SystemSet`] labels automatically attached to dynamic guest systems.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModSystemSet {
    /// Contains all dynamic guest systems across all loaded mods.
    All,
    /// Contains all dynamic guest systems belonging to a specific [`ModId`].
    Mod(ModId),
}

/// Module-scoped Bevy [`SystemSet`] attached to every guest system for intra-mod and cross-mod
/// ordering (`"system_name"` within the same mod, or `"other_mod::system_name"` across mods).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModNamedSystemSet {
    /// Logical module name owning the system.
    pub mod_name: String,
    /// Guest system name registered by the mod.
    pub system_name: String,
    /// Monotonic reload generation for `mod_name` so hot-reloads allocate a fresh
    /// `InternedSystemSet` rather than reusing a removed `SlotMap` key in Bevy `0.20`'s `SystemSets`.
    pub generation: u64,
}

impl ModNamedSystemSet {
    /// Creates a module-scoped named system set at generation `0`.
    #[inline]
    pub fn new(mod_name: impl Into<String>, system_name: impl Into<String>) -> Self {
        Self::with_generation(mod_name, system_name, 0)
    }

    /// Creates a module-scoped named system set with an explicit reload `generation`.
    #[inline]
    pub fn with_generation(
        mod_name: impl Into<String>,
        system_name: impl Into<String>,
        generation: u64,
    ) -> Self {
        Self {
            mod_name: mod_name.into(),
            system_name: system_name.into(),
            generation,
        }
    }

    /// Returns a copy of this named set bound to `generation`.
    #[inline]
    pub fn at_generation(mut self, generation: u64) -> Self {
        self.generation = generation;
        self
    }

    /// Returns `true` if this named system set is currently active in `schedule`'s `ScheduleGraph`.
    ///
    /// Note: Bevy `0.20.1`'s `SystemSets::remove` removes the set from its `SlotMap` (`get`)
    /// without removing the interned entry from its internal `ids` lookup table (`contains`),
    /// so `get_key(...) + get(...)` is the authoritative check for live schedule membership.
    #[inline]
    pub fn is_present_in(&self, schedule: &Schedule) -> bool {
        let sets = &schedule.graph().system_sets;
        sets.get_key(self.clone().intern())
            .and_then(|key| sets.get(key))
            .is_some()
    }

    /// Parses an ordering target string (`"sys"` -> `(default_mod_name, "sys")`,
    /// `"other_mod::sys"` -> `("other_mod", "sys")`) at generation `0`.
    ///
    /// Returns `None` if either segment is empty or if multiple `"::"` separators appear.
    pub fn parse_target(raw: &str, default_mod_name: &str) -> Option<Self> {
        if raw.is_empty() {
            return None;
        }
        if let Some((mod_part, sys_part)) = raw.split_once("::") {
            if mod_part.is_empty() || sys_part.is_empty() || sys_part.contains("::") {
                return None;
            }
            Some(Self::new(mod_part, sys_part))
        } else {
            Some(Self::new(default_mod_name, raw))
        }
    }
}

/// Resource tracking whether Bevy's one-time `Startup` schedule has already completed.
///
/// Pre-inserted with `false` during plugin setup and flipped to `true` in `PostStartup` so
/// tracking startup completion spawns zero additional resource entities during `app.update()`,
/// and any mod loaded after `Startup` runs its `Startup` systems immediately once on load.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct StartupCompleted(pub bool);

impl StartupCompleted {
    /// Returns `true` once `PostStartup` has executed.
    #[inline]
    pub const fn has_run(self) -> bool {
        self.0
    }
}

/// Exclusive `PostStartup` system that marks [`StartupCompleted`] as having executed.
pub fn mark_startup_completed(world: &mut World) {
    if let Some(mut state) = world.get_resource_mut::<StartupCompleted>() {
        state.0 = true;
    } else {
        world.insert_resource(StartupCompleted(true));
    }
}

/// Target Bevy schedule for a dynamic guest system (matches [`ScheduleLabelId`] discriminant values).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum ModSchedule {
    /// Main per-frame `Update` schedule (`0`).
    #[default]
    Update = 0,
    /// One-time `Startup` schedule (`1`).
    Startup = 1,
    /// `PreUpdate` schedule (`2`).
    PreUpdate = 2,
    /// `PostUpdate` schedule (`3`).
    PostUpdate = 3,
    /// Fixed-timestep `FixedUpdate` schedule (`4`).
    FixedUpdate = 4,
    /// End-of-frame `Last` schedule (`5`).
    Last = 5,
}

impl ModSchedule {
    /// Converts a raw `u32` discriminant (`ScheduleLabelId`) into [`ModSchedule`].
    #[inline]
    pub const fn from_raw(raw: u32) -> Option<Self> {
        match raw {
            0 => Some(Self::Update),
            1 => Some(Self::Startup),
            2 => Some(Self::PreUpdate),
            3 => Some(Self::PostUpdate),
            4 => Some(Self::FixedUpdate),
            5 => Some(Self::Last),
            _ => None,
        }
    }

    /// Converts a [`ScheduleLabelId`] into [`ModSchedule`].
    #[inline]
    pub const fn from_label_id(id: ScheduleLabelId) -> Option<Self> {
        Self::from_raw(id.0)
    }

    /// Converts this [`ModSchedule`] into a [`ScheduleLabelId`].
    #[inline]
    pub const fn to_label_id(self) -> ScheduleLabelId {
        ScheduleLabelId(self as u32)
    }

    /// Returns the raw `u32` discriminant value.
    #[inline]
    pub const fn as_raw(self) -> u32 {
        self as u32
    }
}
