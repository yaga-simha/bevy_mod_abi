//! Schedule mapping (`ModScheduleMap`), scoped named ordering sets (`ModNamedSystemSet`),
//! and `&mut World`-native schedule configuration helpers (`B-06`, `L-01`, `L-05`).

use bevy_abi_mod_host_sys::{
    GuestSystem, ModHost, ModId, ModLoadError, ModQuarantine, record_mod_named_sets_in_world,
};
pub use bevy_abi_mod_host_sys::{
    ModNamedSystemSet, ModScheduleMap, ModSystemSet, ModVisibleSets, StartupCompleted,
    resolve_builtin_schedule_id,
};
use bevy_abi_mod_sys::ScheduleId;
use bevy_ecs::schedule::{
    InternedScheduleLabel, InternedSystemSet, IntoScheduleConfigs, Schedule, Schedules,
};
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
/// `before`/`after` ordering targets resolve (`B-06`, `L-05`).
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
        }
    }

    Ok(())
}

/// Registers each `GuestSystem`'s quarantine handle in [`ModQuarantine`] and attaches non-immediate
/// systems into `world`'s [`Schedules`], returning any late `Startup` systems that should execute
/// immediately once (`L-01`, `L-05`, `B-05`, `B-06`).
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
        let sys_name = sys.system_name().to_string();
        let own_set = ModNamedSystemSet::with_generation(mod_name, sys_name.as_str(), own_gen);
        recorded_sets.push((interned, own_set.clone()));

        let after = sys.after_systems().to_vec();
        let before = sys.before_systems().to_vec();

        let mut cfg = sys.in_set(own_set);

        for dep in after {
            if dep.starts_with("host::") {
                if let Some(host_set) = resolve_host_visible_set(world, &dep) {
                    cfg = IntoScheduleConfigs::after(cfg, host_set);
                } else {
                    tracing::warn!(
                        system = %sys_name,
                        mod_name = %mod_name,
                        target = %dep,
                        "guest system declares `after` on unregistered `host::` set"
                    );
                }
                continue;
            }
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "guest system has invalid `after` ordering target"
                );
                continue;
            };
            let target_gen = if parsed.mod_name.as_ref() == mod_name {
                own_gen
            } else {
                resolve_mod_generation_in_world(world, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if !target_exists_in_batch_or_world(world, interned, &batch_sets, &target) {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "guest system declares `after` on unresolved ordering target"
                );
            }
            if target.mod_name.as_ref() == mod_name {
                recorded_sets.push((interned, target.clone()));
            }
            cfg = IntoScheduleConfigs::after(cfg, target);
        }

        for dep in before {
            if dep.starts_with("host::") {
                if let Some(host_set) = resolve_host_visible_set(world, &dep) {
                    cfg = IntoScheduleConfigs::before(cfg, host_set);
                } else {
                    tracing::warn!(
                        system = %sys_name,
                        mod_name = %mod_name,
                        target = %dep,
                        "guest system declares `before` on unregistered `host::` set"
                    );
                }
                continue;
            }
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "guest system has invalid `before` ordering target"
                );
                continue;
            };
            let target_gen = if parsed.mod_name.as_ref() == mod_name {
                own_gen
            } else {
                resolve_mod_generation_in_world(world, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if !target_exists_in_batch_or_world(world, interned, &batch_sets, &target) {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "guest system declares `before` on unresolved ordering target"
                );
            }
            if target.mod_name.as_ref() == mod_name {
                recorded_sets.push((interned, target.clone()));
            }
            cfg = IntoScheduleConfigs::before(cfg, target);
        }

        if let Some(mut schedules) = world.get_resource_mut::<Schedules>() {
            schedules.add_systems(interned, cfg);
        }
    }

    record_mod_named_sets_in_world(world, mod_id, recorded_sets);
    deferred_immediate_startup
}

/// Runs `ScheduleId::STARTUP` systems once immediately when a mod is hot-loaded after Bevy's
/// `Startup` schedule has already executed (`L-01`).
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
        let sys_name = sys.system_name().to_string();
        let own_set = ModNamedSystemSet::with_generation(mod_name, sys_name.as_str(), own_gen);
        let after = sys.after_systems().to_vec();
        let before = sys.before_systems().to_vec();

        let mut cfg = sys.in_set(own_set);
        for dep in after {
            if dep.starts_with("host::") {
                if let Some(host_set) = resolve_host_visible_set(world, &dep) {
                    cfg = IntoScheduleConfigs::after(cfg, host_set);
                }
                continue;
            }
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "startup system has invalid `after` ordering target"
                );
                continue;
            };
            let target_gen = if parsed.mod_name.as_ref() == mod_name {
                own_gen
            } else {
                resolve_mod_generation_in_world(world, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if target.mod_name.as_ref() == mod_name
                && !batch_names.contains(target.system_name.as_ref())
            {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "startup system declares `after` on unresolved startup target"
                );
            }
            cfg = IntoScheduleConfigs::after(cfg, target);
        }
        for dep in before {
            if dep.starts_with("host::") {
                if let Some(host_set) = resolve_host_visible_set(world, &dep) {
                    cfg = IntoScheduleConfigs::before(cfg, host_set);
                }
                continue;
            }
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "startup system has invalid `before` ordering target"
                );
                continue;
            };
            let target_gen = if parsed.mod_name.as_ref() == mod_name {
                own_gen
            } else {
                resolve_mod_generation_in_world(world, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if target.mod_name.as_ref() == mod_name
                && !batch_names.contains(target.system_name.as_ref())
            {
                tracing::warn!(
                    system = %sys_name,
                    mod_name = %mod_name,
                    target = %dep,
                    "startup system declares `before` on unresolved startup target"
                );
            }
            cfg = IntoScheduleConfigs::before(cfg, target);
        }
        temp_schedule.add_systems(cfg);
    }

    temp_schedule.run(world);
}
