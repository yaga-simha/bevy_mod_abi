//! Schedule mapping (`ModScheduleMap`), scoped named ordering sets (`ModNamedSystemSet`),
//! and `&mut World`-native schedule configuration helpers.

use bevy_abi_mod_host_sys::schedule::{ModScheduleSetRegistry, install_mod_schedule_set_pass};
use bevy_abi_mod_host_sys::{
    GuestSystem, ModHost, ModId, ModLoadError, ModQuarantine, record_mod_named_sets_in_world,
};
pub use bevy_abi_mod_host_sys::{
    ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
    resolve_builtin_schedule_id,
};
use bevy_abi_mod_sys::ScheduleId;
use bevy_ecs::schedule::{
    InternedScheduleLabel, InternedSystemSet, IntoScheduleConfigs, Schedule, ScheduleConfigs,
    Schedules, SystemSet,
};
use bevy_ecs::system::ScheduleSystem;
use bevy_ecs::world::World;
use bevy_platform::collections::HashSet;
use std::path::Path;

fn resolve_schedule_in_world(world: &World, id: ScheduleId) -> Option<InternedScheduleLabel> {
    if let Some(builtin) = resolve_builtin_schedule_id(id) {
        return Some(builtin);
    }
    world
        .get_resource::<ModScheduleMap>()
        .and_then(|map| map.resolve(id))
}

fn resolve_mod_generation_in_world(world: &World, mod_name: &str) -> u64 {
    world
        .get_resource::<ModHost>()
        .map(|h| h.mod_generation(mod_name))
        .unwrap_or(0)
}

fn resolve_host_visible_set(world: &World, raw_dep: &str) -> Option<InternedSystemSet> {
    let stripped = raw_dep.strip_prefix("host::")?;
    world
        .get_resource::<ModVisibleSets>()
        .and_then(|sets| sets.resolve(stripped))
}

fn target_exists_in_batch_or_world(
    world: &World,
    label: InternedScheduleLabel,
    batch_sets: &HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    target: &ModNamedSystemSet,
) -> bool {
    if batch_sets.contains(&(label, target.clone())) {
        return true;
    }
    if let Some(schedules) = world.get_resource::<Schedules>()
        && let Some(sched) = schedules.get(label)
        && target.is_present_in(sched)
    {
        return true;
    }
    if let Some(host) = world.get_resource::<ModHost>() {
        return host
            .loaded_mods()
            .iter()
            .any(|m| m.is_active() && m.mod_name() == target.mod_name.as_ref());
    }
    false
}

/// Validates that all systems in `systems` target built-in schedules (`0..=5`) or custom schedules
/// (`>= 1024`) registered in `world`'s [`ModScheduleMap`], and (when `strict == true`) that all
/// `before`/`after` ordering targets resolve.
pub(crate) fn prevalidate_mod_schedules_and_ordering(
    world: &World,
    path: &Path,
    mod_name: &str,
    strict: bool,
    systems: &[GuestSystem],
) -> Result<(), ModLoadError> {
    let startup_already_ran = world
        .get_resource::<StartupCompleted>()
        .is_some_and(|s| s.has_run());
    let own_gen = resolve_mod_generation_in_world(world, mod_name);

    let mut batch_sets: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = HashSet::default();
    let mut late_startup_names: HashSet<&str> = HashSet::default();
    let mut resolved_systems: Vec<(&GuestSystem, InternedScheduleLabel)> =
        Vec::with_capacity(if strict { systems.len() } else { 0 });

    for sys in systems {
        let Some(interned) = resolve_schedule_in_world(world, sys.schedule()) else {
            return Err(ModLoadError::UnregisteredSchedule {
                path: path.to_path_buf(),
                system_name: sys.system_name().to_string(),
                schedule: sys.schedule(),
            });
        };
        if startup_already_ran && sys.schedule() == ScheduleId::STARTUP {
            late_startup_names.insert(sys.system_name());
        } else {
            batch_sets.insert((
                interned,
                ModNamedSystemSet::with_generation(mod_name, sys.system_name(), own_gen),
            ));
        }
        if strict {
            resolved_systems.push((sys, interned));
        }
    }

    if strict {
        let unresolved = |sys: &GuestSystem, target: &str| ModLoadError::UnresolvedOrderingTarget {
            path: path.to_path_buf(),
            system_name: sys.system_name().to_string(),
            target: target.to_string(),
        };
        for (sys, interned) in resolved_systems {
            let is_late_startup = startup_already_ran && sys.schedule() == ScheduleId::STARTUP;
            for dep in sys
                .after_systems()
                .iter()
                .chain(sys.before_systems().iter())
            {
                if dep.starts_with("host::") {
                    if resolve_host_visible_set(world, dep).is_none() {
                        return Err(unresolved(sys, dep));
                    }
                    continue;
                }
                let Some(parsed) = ModNamedSystemSet::parse_target(dep, mod_name) else {
                    return Err(unresolved(sys, dep));
                };
                let target_gen = if parsed.mod_name.as_ref() == mod_name {
                    own_gen
                } else {
                    resolve_mod_generation_in_world(world, &parsed.mod_name)
                };
                let target = parsed.at_generation(target_gen);
                let resolved = if is_late_startup && target.mod_name.as_ref() == mod_name {
                    late_startup_names.contains(target.system_name.as_ref())
                } else {
                    target_exists_in_batch_or_world(world, interned, &batch_sets, &target)
                };
                if !resolved {
                    return Err(unresolved(sys, dep));
                }
            }
            for target_set in sys.in_sets() {
                let valid = if target_set.starts_with("host::") {
                    resolve_host_visible_set(world, target_set).is_some()
                } else {
                    ModNamedSystemSet::parse_target(target_set, mod_name).is_some()
                };
                if !valid {
                    return Err(unresolved(sys, target_set));
                }
            }
            // Weak ordering (`after_weak`/`before_weak`) and `ambiguous_with` only require valid
            // target syntax: absent/unloaded targets MUST NOT fail strict validation.
            for weak_or_ambig in sys
                .after_weak_systems()
                .iter()
                .chain(sys.before_weak_systems().iter())
                .chain(sys.ambiguous_with_systems().iter())
            {
                let valid = if let Some(host_part) = weak_or_ambig.strip_prefix("host::") {
                    !host_part.is_empty() && !host_part.contains("::")
                } else {
                    ModNamedSystemSet::parse_target(weak_or_ambig, mod_name).is_some()
                };
                if !valid {
                    return Err(unresolved(sys, weak_or_ambig));
                }
            }
        }
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum OrderingEdgeKind {
    InSet,
    After,
    Before,
    AfterWeak,
    BeforeWeak,
    AmbiguousWith,
}

impl OrderingEdgeKind {
    const fn label(self) -> &'static str {
        match self {
            Self::InSet => "in_set",
            Self::After => "after",
            Self::Before => "before",
            Self::AfterWeak => "after_weak",
            Self::BeforeWeak => "before_weak",
            Self::AmbiguousWith => "ambiguous_with",
        }
    }

    const fn is_strong_ordering(self) -> bool {
        matches!(self, Self::After | Self::Before)
    }

    const fn is_optional_target(self) -> bool {
        matches!(
            self,
            Self::AfterWeak | Self::BeforeWeak | Self::AmbiguousWith
        )
    }

    fn apply<S: SystemSet>(
        self,
        cfg: ScheduleConfigs<ScheduleSystem>,
        set: S,
    ) -> ScheduleConfigs<ScheduleSystem> {
        match self {
            Self::InSet => IntoScheduleConfigs::in_set(cfg, set),
            Self::After => IntoScheduleConfigs::after(cfg, set),
            Self::Before => IntoScheduleConfigs::before(cfg, set),
            Self::AfterWeak => IntoScheduleConfigs::after_weak(cfg, set),
            Self::BeforeWeak => IntoScheduleConfigs::before_weak(cfg, set),
            Self::AmbiguousWith => IntoScheduleConfigs::ambiguous_with(cfg, set),
        }
    }
}

struct OrderingCtx<'a> {
    world: &'a World,
    mod_name: &'a str,
    own_gen: u64,
    check_batch: Option<(
        InternedScheduleLabel,
        &'a HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
    batch_names: Option<&'a HashSet<String>>,
    recorded_sets: Option<(
        InternedScheduleLabel,
        &'a mut Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
}

impl OrderingCtx<'_> {
    fn apply_edge(
        &mut self,
        sys_name: &str,
        kind: OrderingEdgeKind,
        raw_target: &str,
        cfg: ScheduleConfigs<ScheduleSystem>,
    ) -> ScheduleConfigs<ScheduleSystem> {
        if raw_target.starts_with("host::") {
            if let Some(host_set) = resolve_host_visible_set(self.world, raw_target) {
                // Do not add `host::*` sets to `recorded_sets` so mod unload never excises
                // host-owned system sets or native host systems.
                return kind.apply(cfg, host_set);
            }
            if !kind.is_optional_target() {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %self.mod_name,
                    edge = kind.label(),
                    target = %raw_target,
                    "guest system targets unregistered `host::` set"
                );
            }
            return cfg;
        }

        let Some(parsed) = ModNamedSystemSet::parse_target(raw_target, self.mod_name) else {
            tracing::warn!(
                system = %sys_name,
                mod_name = %self.mod_name,
                edge = kind.label(),
                target = %raw_target,
                "guest system has invalid ordering target syntax"
            );
            return cfg;
        };

        let target_gen = if parsed.mod_name.as_ref() == self.mod_name {
            self.own_gen
        } else {
            resolve_mod_generation_in_world(self.world, &parsed.mod_name)
        };
        let target = parsed.at_generation(target_gen);

        if kind.is_strong_ordering() {
            if let Some((label, batch_sets)) = self.check_batch {
                if !target_exists_in_batch_or_world(self.world, label, batch_sets, &target) {
                    tracing::warn!(
                        system = %sys_name,
                        mod_name = %self.mod_name,
                        edge = kind.label(),
                        target = %raw_target,
                        "guest system declares ordering on unresolved target"
                    );
                }
            } else if let Some(names) = self.batch_names
                && target.mod_name.as_ref() == self.mod_name
                && !names.contains(target.system_name.as_ref())
            {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %self.mod_name,
                    edge = kind.label(),
                    target = %raw_target,
                    "startup system declares ordering on unresolved startup target"
                );
            }
        }

        if target.mod_name.as_ref() == self.mod_name
            && let Some((label, ref mut sets_out)) = self.recorded_sets
        {
            sets_out.push((label, target.clone()));
        }

        kind.apply(cfg, target)
    }

    fn build_configured_system(&mut self, sys: GuestSystem) -> ScheduleConfigs<ScheduleSystem> {
        let sys_name = sys.system_name().to_string();
        let own_set =
            ModNamedSystemSet::with_generation(self.mod_name, sys_name.as_str(), self.own_gen);
        if let Some((label, ref mut sets_out)) = self.recorded_sets {
            sets_out.push((label, own_set.clone()));
        }

        let edge_lists: [(OrderingEdgeKind, Vec<String>); 6] = [
            (OrderingEdgeKind::InSet, sys.in_sets().to_vec()),
            (OrderingEdgeKind::After, sys.after_systems().to_vec()),
            (OrderingEdgeKind::Before, sys.before_systems().to_vec()),
            (
                OrderingEdgeKind::AfterWeak,
                sys.after_weak_systems().to_vec(),
            ),
            (
                OrderingEdgeKind::BeforeWeak,
                sys.before_weak_systems().to_vec(),
            ),
            (
                OrderingEdgeKind::AmbiguousWith,
                sys.ambiguous_with_systems().to_vec(),
            ),
        ];
        let ambiguous_with_all = sys.is_ambiguous_with_all();

        let mut cfg = sys.in_set(own_set);
        for (kind, targets) in edge_lists {
            for raw_target in targets {
                cfg = self.apply_edge(&sys_name, kind, &raw_target, cfg);
            }
        }

        if ambiguous_with_all {
            cfg = IntoScheduleConfigs::ambiguous_with_all(cfg);
        }
        cfg
    }
}

/// Configures a standalone [`GuestSystem`] with all declared [`ModNamedSystemSet`]s, strong/weak
/// ordering edges, and ambiguity exclusions against `world`.
pub fn configure_guest_system_in_world(
    world: &World,
    mod_name: &str,
    own_gen: u64,
    sys: GuestSystem,
) -> ScheduleConfigs<ScheduleSystem> {
    OrderingCtx {
        world,
        mod_name,
        own_gen,
        check_batch: None,
        batch_names: None,
        recorded_sets: None,
    }
    .build_configured_system(sys)
}

/// Applies Bevy's `.chain_weak()` ordering across an ordered sequence of [`ModNamedSystemSet`]s
/// inside `schedule` on `world`.
pub fn configure_weak_set_chain(
    world: &mut World,
    schedule: InternedScheduleLabel,
    sets: &[ModNamedSystemSet],
) {
    if sets.len() < 2 {
        return;
    }
    world.init_resource::<Schedules>();
    if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
        for pair in sets.windows(2) {
            schedules.configure_sets(schedule, (pair[0].clone(), pair[1].clone()).chain_weak());
        }
    }
}

/// Registers each `GuestSystem`'s quarantine handle in [`ModQuarantine`] and attaches non-immediate
/// systems into `world`'s [`Schedules`], returning any late `Startup` systems that should execute
/// immediately once.
pub(crate) fn schedule_mod_systems_in_world(
    world: &mut World,
    mod_id: ModId,
    mod_name: &str,
    systems: Vec<GuestSystem>,
) -> Vec<GuestSystem> {
    world.init_resource::<ModQuarantine>();
    if let Some(mut quarantine) = world.get_resource_mut::<ModQuarantine>() {
        for sys in &systems {
            quarantine.register_system(mod_id, sys.system_name_arc(), sys.enabled_flag());
        }
    }

    let startup_already_ran = world
        .get_resource::<StartupCompleted>()
        .is_some_and(|s| s.has_run());
    let own_gen = resolve_mod_generation_in_world(world, mod_name);

    let mut deferred_immediate_startup = Vec::new();
    let mut to_schedule: Vec<(ScheduleId, InternedScheduleLabel, GuestSystem)> =
        Vec::with_capacity(systems.len());

    for sys in systems {
        let sched_id = sys.schedule();
        if startup_already_ran && sched_id == ScheduleId::STARTUP {
            deferred_immediate_startup.push(sys);
        } else if let Some(interned) = resolve_schedule_in_world(world, sched_id) {
            to_schedule.push((sched_id, interned, sys));
        }
    }

    let batch_sets: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = to_schedule
        .iter()
        .map(|(_, interned, s)| {
            (
                *interned,
                ModNamedSystemSet::with_generation(mod_name, s.system_name(), own_gen),
            )
        })
        .collect();

    world.init_resource::<Schedules>();

    let has_schedule_sets = world.contains_resource::<ModScheduleSetRegistry>();
    let mut recorded_sets = Vec::with_capacity(to_schedule.len());
    for (sched_id, interned, sys) in to_schedule {
        let cfg = OrderingCtx {
            world,
            mod_name,
            own_gen,
            check_batch: Some((interned, &batch_sets)),
            batch_names: None,
            recorded_sets: Some((interned, &mut recorded_sets)),
        }
        .build_configured_system(sys);

        if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
            if has_schedule_sets {
                let schedule = schedules.entry(interned);
                install_mod_schedule_set_pass(schedule, sched_id);
            }
            schedules.add_systems(interned, cfg);
        }
    }

    record_mod_named_sets_in_world(world, mod_id, recorded_sets);
    deferred_immediate_startup
}

/// Runs `ScheduleId::STARTUP` systems once immediately when a mod is hot-loaded after Bevy's
/// `Startup` schedule has already executed.
pub(crate) fn run_late_startup_systems_in_world(
    world: &mut World,
    mod_name: &str,
    startup_systems: Vec<GuestSystem>,
) {
    if startup_systems.is_empty() {
        return;
    }

    let own_gen = resolve_mod_generation_in_world(world, mod_name);
    let batch_names: HashSet<String> = startup_systems
        .iter()
        .map(|s| s.system_name().to_string())
        .collect();

    let mut temp_schedule = Schedule::default();
    let mut ctx = OrderingCtx {
        world,
        mod_name,
        own_gen,
        check_batch: None,
        batch_names: Some(&batch_names),
        recorded_sets: None,
    };
    for sys in startup_systems {
        temp_schedule.add_systems(ctx.build_configured_system(sys));
    }

    temp_schedule.run(world);
}
