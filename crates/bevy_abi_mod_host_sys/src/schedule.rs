//! Schedule mapping (`ModScheduleMap`), system sets, and startup tracking (`B-06`).

use crate::loader::ModId;
use bevy_abi_mod_sys::ScheduleId;
use bevy_app::{FixedUpdate, Last, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::prelude::Resource;
use bevy_ecs::schedule::{
    InternedScheduleLabel, InternedSystemSet, Schedule, ScheduleLabel, SystemSet,
};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use std::sync::Arc;

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
    pub mod_name: Arc<str>,
    /// Guest system name registered by the mod.
    pub system_name: Arc<str>,
    /// Monotonic reload generation for `mod_name` so hot-reloads allocate a fresh
    /// `InternedSystemSet` rather than reusing a removed `SlotMap` key in Bevy `0.20`'s `SystemSets`.
    pub generation: u64,
}

impl ModNamedSystemSet {
    /// Creates a module-scoped named system set at generation `0`.
    #[inline]
    pub fn new(mod_name: impl Into<Arc<str>>, system_name: impl Into<Arc<str>>) -> Self {
        Self::with_generation(mod_name, system_name, 0)
    }

    /// Creates a module-scoped named system set with an explicit reload `generation`.
    #[inline]
    pub fn with_generation(
        mod_name: impl Into<Arc<str>>,
        system_name: impl Into<Arc<str>>,
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

/// Resource mapping ABI [`ScheduleId`] discriminants (`0..=5` built-in and `>= 1024` custom game
/// schedules) onto Bevy [`InternedScheduleLabel`]s (`B-06`, ADR 0002).
#[derive(Resource, Default, Clone, Debug)]
pub struct ModScheduleMap {
    custom: HashMap<ScheduleId, InternedScheduleLabel>,
}

impl ModScheduleMap {
    /// Creates an empty custom schedule map (built-in IDs `0..=5` are always available).
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a game-defined custom [`ScheduleId`] (`id.0 >= ScheduleId::CUSTOM_BASE` = `1024`)
    /// mapped to `label`.
    ///
    /// Returns `false` if `id` is not in the custom range (`< 1024`).
    #[inline]
    pub fn register<S: ScheduleLabel>(&mut self, id: ScheduleId, label: S) -> bool {
        if !id.is_custom() {
            return false;
        }
        self.custom.insert(id, label.intern());
        true
    }

    /// Resolves `id` to its [`InternedScheduleLabel`], handling built-in IDs (`0..=5`) directly
    /// and consulting registered custom entries for `id.0 >= 1024`.
    #[inline]
    pub fn resolve(&self, id: ScheduleId) -> Option<InternedScheduleLabel> {
        if let Some(builtin) = resolve_builtin_schedule_id(id) {
            return Some(builtin);
        }
        self.custom.get(&id).copied()
    }

    /// Returns `true` if `id` is a built-in schedule (`0..=5`) or a registered custom schedule.
    #[inline]
    pub fn contains(&self, id: ScheduleId) -> bool {
        id.is_builtin() || self.custom.contains_key(&id)
    }
}

/// Resource mapping logical host set names (referenced by guests as `"host::<name>"`) onto Bevy
/// [`InternedSystemSet`]s so guest systems can order themselves `.after(c"host::physics")` or
/// `.before(c"host::render")` without risking host-set excision on mod unload (`F-05`).
#[derive(Resource, Default, Clone, Debug)]
pub struct ModVisibleSets {
    sets: HashMap<Arc<str>, InternedSystemSet>,
}

impl ModVisibleSets {
    /// Creates an empty [`ModVisibleSets`] map.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Exposes `set` to guest ordering targets under `"host::<name>"` (accepts either bare `"physics"`
    /// or `"host::physics"`).
    ///
    /// Returns `false` if `name` is empty or contains `"::"` other than a single leading `"host::"`.
    pub fn register<S: SystemSet>(&mut self, name: &str, set: S) -> bool {
        let stripped = name.strip_prefix("host::").unwrap_or(name);
        if stripped.is_empty() || stripped.contains("::") {
            return false;
        }
        self.sets.insert(Arc::from(stripped), set.intern());
        true
    }

    /// Resolves a bare host set name (`"physics"`) or prefixed `"host::physics"` to its [`InternedSystemSet`].
    #[inline]
    pub fn resolve(&self, name: &str) -> Option<InternedSystemSet> {
        let stripped = name.strip_prefix("host::").unwrap_or(name);
        self.sets.get(stripped).copied()
    }

    /// Returns `true` if `name` (bare or `"host::"`-prefixed) is registered.
    #[inline]
    pub fn contains(&self, name: &str) -> bool {
        self.resolve(name).is_some()
    }
}

/// Resolves one of the six built-in ABI [`ScheduleId`]s (`0..=5`) into its Bevy [`InternedScheduleLabel`].
#[inline]
pub fn resolve_builtin_schedule_id(id: ScheduleId) -> Option<InternedScheduleLabel> {
    match id {
        ScheduleId::UPDATE => Some(Update.intern()),
        ScheduleId::STARTUP => Some(Startup.intern()),
        ScheduleId::PRE_UPDATE => Some(PreUpdate.intern()),
        ScheduleId::POST_UPDATE => Some(PostUpdate.intern()),
        ScheduleId::FIXED_UPDATE => Some(FixedUpdate.intern()),
        ScheduleId::LAST => Some(Last.intern()),
        _ => None,
    }
}

/// Resource tracking whether Bevy's one-time `Startup` schedule has already completed.
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
