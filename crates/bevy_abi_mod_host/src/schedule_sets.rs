//! Schedule mapping (`ModScheduleMap`), scoped named ordering sets (`ModNamedSystemSet`),
//! and `&mut World`-native schedule configuration helpers.

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
    Schedules,
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
    }

    if strict {
        for sys in systems {
            let interned = resolve_schedule_in_world(world, sys.schedule()).expect("checked above");
            let is_late_startup = startup_already_ran && sys.schedule() == ScheduleId::STARTUP;
            for dep in sys
                .after_systems()
                .iter()
                .chain(sys.before_systems().iter())
            {
                if dep.starts_with("host::") {
                    if resolve_host_visible_set(world, dep).is_none() {
                        return Err(ModLoadError::UnresolvedOrderingTarget {
                            path: path.to_path_buf(),
                            system_name: sys.system_name().to_string(),
                            target: dep.clone(),
                        });
                    }
                    continue;
                }
                let Some(parsed) = ModNamedSystemSet::parse_target(dep, mod_name) else {
                    return Err(ModLoadError::UnresolvedOrderingTarget {
                        path: path.to_path_buf(),
                        system_name: sys.system_name().to_string(),
                        target: dep.clone(),
                    });
                };
                let target_gen = if parsed.mod_name.as_ref() == mod_name {
                    own_gen
                } else {
                    resolve_mod_generation_in_world(world, &parsed.mod_name)
                };
                let target = parsed.at_generation(target_gen);
                let resolved = if is_late_startup {
                    if target.mod_name.as_ref() == mod_name {
                        late_startup_names.contains(target.system_name.as_ref())
                    } else {
                        target_exists_in_batch_or_world(world, interned, &batch_sets, &target)
                    }
                } else {
                    target_exists_in_batch_or_world(world, interned, &batch_sets, &target)
                };
                if !resolved {
                    return Err(ModLoadError::UnresolvedOrderingTarget {
                        path: path.to_path_buf(),
                        system_name: sys.system_name().to_string(),
                        target: dep.clone(),
                    });
                }
            }
            for target_set in sys.in_sets() {
                if target_set.starts_with("host::") {
                    if resolve_host_visible_set(world, target_set).is_none() {
                        return Err(ModLoadError::UnresolvedOrderingTarget {
                            path: path.to_path_buf(),
                            system_name: sys.system_name().to_string(),
                            target: target_set.clone(),
                        });
                    }
                } else if ModNamedSystemSet::parse_target(target_set, mod_name).is_none() {
                    return Err(ModLoadError::UnresolvedOrderingTarget {
                        path: path.to_path_buf(),
                        system_name: sys.system_name().to_string(),
                        target: target_set.clone(),
                    });
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
                if let Some(host_part) = weak_or_ambig.strip_prefix("host::") {
                    if host_part.is_empty() || host_part.contains("::") {
                        return Err(ModLoadError::UnresolvedOrderingTarget {
                            path: path.to_path_buf(),
                            system_name: sys.system_name().to_string(),
                            target: weak_or_ambig.clone(),
                        });
                    }
                } else if ModNamedSystemSet::parse_target(weak_or_ambig, mod_name).is_none() {
                    return Err(ModLoadError::UnresolvedOrderingTarget {
                        path: path.to_path_buf(),
                        system_name: sys.system_name().to_string(),
                        target: weak_or_ambig.clone(),
                    });
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
}

#[allow(clippy::too_many_arguments)]
fn apply_ordering_edge(
    world: &World,
    mod_name: &str,
    own_gen: u64,
    sys_name: &str,
    kind: OrderingEdgeKind,
    raw_target: &str,
    check_batch: Option<(
        InternedScheduleLabel,
        &HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
    batch_names: Option<&HashSet<String>>,
    mut cfg: ScheduleConfigs<ScheduleSystem>,
    mut recorded_sets: Option<(
        InternedScheduleLabel,
        &mut Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
) -> ScheduleConfigs<ScheduleSystem> {
    if raw_target.starts_with("host::") {
        if let Some(host_set) = resolve_host_visible_set(world, raw_target) {
            // Do not add `host::*` sets to `recorded_sets` so mod unload never excises
            // host-owned system sets or native host systems.
            cfg = match kind {
                OrderingEdgeKind::InSet => IntoScheduleConfigs::in_set(cfg, host_set),
                OrderingEdgeKind::After => IntoScheduleConfigs::after(cfg, host_set),
                OrderingEdgeKind::Before => IntoScheduleConfigs::before(cfg, host_set),
                OrderingEdgeKind::AfterWeak => IntoScheduleConfigs::after_weak(cfg, host_set),
                OrderingEdgeKind::BeforeWeak => IntoScheduleConfigs::before_weak(cfg, host_set),
                OrderingEdgeKind::AmbiguousWith => {
                    IntoScheduleConfigs::ambiguous_with(cfg, host_set)
                }
            };
        } else if !kind.is_optional_target() {
            tracing::warn!(
                system = %sys_name,
                mod_name = %mod_name,
                edge = kind.label(),
                target = %raw_target,
                "guest system targets unregistered `host::` set"
            );
        }
        return cfg;
    }

    let Some(parsed) = ModNamedSystemSet::parse_target(raw_target, mod_name) else {
        tracing::warn!(
            system = %sys_name,
            mod_name = %mod_name,
            edge = kind.label(),
            target = %raw_target,
            "guest system has invalid ordering target syntax"
        );
        return cfg;
    };

    let target_gen = if parsed.mod_name.as_ref() == mod_name {
        own_gen
    } else {
        resolve_mod_generation_in_world(world, &parsed.mod_name)
    };
    let target = parsed.at_generation(target_gen);

    if kind.is_strong_ordering() {
        if let Some((label, batch_sets)) = check_batch {
            if !target_exists_in_batch_or_world(world, label, batch_sets, &target) {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    edge = kind.label(),
                    target = %raw_target,
                    "guest system declares ordering on unresolved target"
                );
            }
        } else if let Some(names) = batch_names
            && target.mod_name.as_ref() == mod_name
            && !names.contains(target.system_name.as_ref())
        {
            tracing::warn!(
                system = %sys_name,
                mod_name = %mod_name,
                edge = kind.label(),
                target = %raw_target,
                "startup system declares ordering on unresolved startup target"
            );
        }
    }

    if target.mod_name.as_ref() == mod_name
        && let Some((label, ref mut sets_out)) = recorded_sets
    {
        sets_out.push((label, target.clone()));
    }

    match kind {
        OrderingEdgeKind::InSet => IntoScheduleConfigs::in_set(cfg, target),
        OrderingEdgeKind::After => IntoScheduleConfigs::after(cfg, target),
        OrderingEdgeKind::Before => IntoScheduleConfigs::before(cfg, target),
        OrderingEdgeKind::AfterWeak => IntoScheduleConfigs::after_weak(cfg, target),
        OrderingEdgeKind::BeforeWeak => IntoScheduleConfigs::before_weak(cfg, target),
        OrderingEdgeKind::AmbiguousWith => IntoScheduleConfigs::ambiguous_with(cfg, target),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_configured_guest_system(
    world: &World,
    mod_name: &str,
    own_gen: u64,
    sys: GuestSystem,
    check_batch: Option<(
        InternedScheduleLabel,
        &HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
    batch_names: Option<&HashSet<String>>,
    mut recorded_sets: Option<(
        InternedScheduleLabel,
        &mut Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
    )>,
) -> ScheduleConfigs<ScheduleSystem> {
    let sys_name = sys.system_name().to_string();
    let own_set = ModNamedSystemSet::with_generation(mod_name, sys_name.as_str(), own_gen);
    if let Some((label, ref mut sets_out)) = recorded_sets {
        sets_out.push((label, own_set.clone()));
    }

    let in_sets = sys.in_sets().to_vec();
    let after = sys.after_systems().to_vec();
    let before = sys.before_systems().to_vec();
    let after_weak = sys.after_weak_systems().to_vec();
    let before_weak = sys.before_weak_systems().to_vec();
    let ambiguous_with = sys.ambiguous_with_systems().to_vec();
    let ambiguous_with_all = sys.is_ambiguous_with_all();

    let mut cfg = sys.in_set(own_set);

    let edge_lists: [(OrderingEdgeKind, Vec<String>); 6] = [
        (OrderingEdgeKind::InSet, in_sets),
        (OrderingEdgeKind::After, after),
        (OrderingEdgeKind::Before, before),
        (OrderingEdgeKind::AfterWeak, after_weak),
        (OrderingEdgeKind::BeforeWeak, before_weak),
        (OrderingEdgeKind::AmbiguousWith, ambiguous_with),
    ];

    for (kind, targets) in edge_lists {
        for raw_target in targets {
            let rec_reborrow = recorded_sets
                .as_mut()
                .map(|(lbl, vec_ref)| (*lbl, &mut **vec_ref));
            cfg = apply_ordering_edge(
                world,
                mod_name,
                own_gen,
                &sys_name,
                kind,
                &raw_target,
                check_batch,
                batch_names,
                cfg,
                rec_reborrow,
            );
        }
    }

    if ambiguous_with_all {
        cfg = IntoScheduleConfigs::ambiguous_with_all(cfg);
    }

    cfg
}

/// Configures a standalone [`GuestSystem`] with all declared [`ModNamedSystemSet`]s, strong/weak
/// ordering edges, and ambiguity exclusions against `world`.
pub fn configure_guest_system_in_world(
    world: &World,
    mod_name: &str,
    own_gen: u64,
    sys: GuestSystem,
) -> ScheduleConfigs<ScheduleSystem> {
    build_configured_guest_system(world, mod_name, own_gen, sys, None, None, None)
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
    if !world.contains_resource::<Schedules>() {
        world.insert_resource(Schedules::new());
    }
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
    if !world.contains_resource::<ModQuarantine>() {
        world.insert_resource(ModQuarantine::new());
    }
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
    let mut to_schedule: Vec<(InternedScheduleLabel, GuestSystem)> =
        Vec::with_capacity(systems.len());

    for sys in systems {
        if startup_already_ran && sys.schedule() == ScheduleId::STARTUP {
            deferred_immediate_startup.push(sys);
        } else if let Some(interned) = resolve_schedule_in_world(world, sys.schedule()) {
            to_schedule.push((interned, sys));
        }
    }

    let batch_sets: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = to_schedule
        .iter()
        .map(|(interned, s)| {
            (
                *interned,
                ModNamedSystemSet::with_generation(mod_name, s.system_name(), own_gen),
            )
        })
        .collect();

    if !world.contains_resource::<Schedules>() {
        world.insert_resource(Schedules::new());
    }

    let mut recorded_sets = Vec::with_capacity(to_schedule.len());
    for (interned, sys) in to_schedule {
        let cfg = build_configured_guest_system(
            world,
            mod_name,
            own_gen,
            sys,
            Some((interned, &batch_sets)),
            None,
            Some((interned, &mut recorded_sets)),
        );

        if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
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
    for sys in startup_systems {
        let cfg = build_configured_guest_system(
            world,
            mod_name,
            own_gen,
            sys,
            None,
            Some(&batch_names),
            None,
        );
        temp_schedule.add_systems(cfg);
    }

    temp_schedule.run(world);
}
