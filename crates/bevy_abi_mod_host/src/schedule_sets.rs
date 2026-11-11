//! Schedule labels, scoped named ordering sets, and schedule configuration helpers.

use bevy_abi_mod_host_sys::{DynamicCSystem, ModHost, ModId, record_mod_named_sets_in_world};
pub use bevy_abi_mod_host_sys::{ModNamedSystemSet, ModSchedule, ModSystemSet, StartupCompleted};
use bevy_app::{App, FixedUpdate, Last, PostUpdate, PreUpdate, Startup, Update};
use bevy_ecs::schedule::{
    InternedScheduleLabel, IntoScheduleConfigs, Schedule, ScheduleLabel, Schedules,
};
use std::collections::HashSet;

pub(crate) fn interned_label_for_mod_schedule(schedule: ModSchedule) -> InternedScheduleLabel {
    match schedule {
        ModSchedule::Update => Update.intern(),
        ModSchedule::Startup => Startup.intern(),
        ModSchedule::PreUpdate => PreUpdate.intern(),
        ModSchedule::PostUpdate => PostUpdate.intern(),
        ModSchedule::FixedUpdate => FixedUpdate.intern(),
        ModSchedule::Last => Last.intern(),
    }
}

fn resolve_mod_generation(app: &App, mod_name: &str) -> u64 {
    app.world()
        .get_resource::<ModHost>()
        .map(|h| h.mod_generation(mod_name))
        .unwrap_or(0)
}

fn target_exists_in_batch_or_world(
    app: &App,
    label: InternedScheduleLabel,
    batch_sets: &HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    target: &ModNamedSystemSet,
) -> bool {
    if batch_sets.contains(&(label, target.clone())) {
        return true;
    }
    if let Some(schedules) = app.world().get_resource::<Schedules>()
        && let Some(sched) = schedules.get(label)
        && target.is_present_in(sched)
    {
        return true;
    }
    if let Some(host) = app.world().get_resource::<ModHost>() {
        return host
            .loaded_mods()
            .iter()
            .any(|m| m.is_active() && m.mod_name() == target.mod_name);
    }
    false
}

fn add_configured_system_to_label<S: ScheduleLabel + Clone>(
    app: &mut App,
    label: S,
    mod_name: &str,
    own_gen: u64,
    sys: DynamicCSystem,
    batch_sets: &HashSet<(InternedScheduleLabel, ModNamedSystemSet)>,
    recorded_sets: &mut Vec<(InternedScheduleLabel, ModNamedSystemSet)>,
) {
    let interned = label.intern();
    let own_set = ModNamedSystemSet::with_generation(mod_name, sys.system_name(), own_gen);
    recorded_sets.push((interned, own_set.clone()));

    let sys_name = sys.system_name().to_string();
    let after = sys.after_systems().to_vec();
    let before = sys.before_systems().to_vec();

    // Every scheduled guest system unconditionally joins its scoped `ModNamedSystemSet`.
    let mut cfg = sys.in_set(own_set);

    for dep in after {
        let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
            eprintln!(
                "[bevy_abi_mod] warning: system '{sys_name}' in mod '{mod_name}' has invalid empty `after` target '{dep}'"
            );
            continue;
        };
        let target_gen = if parsed.mod_name == mod_name {
            own_gen
        } else {
            resolve_mod_generation(app, &parsed.mod_name)
        };
        let target = parsed.at_generation(target_gen);
        if !target_exists_in_batch_or_world(app, interned, batch_sets, &target) {
            eprintln!(
                "[bevy_abi_mod] warning: system '{sys_name}' in mod '{mod_name}' declares `after` on unresolved target '{dep}'"
            );
        }
        if target.mod_name == mod_name {
            recorded_sets.push((interned, target.clone()));
        }
        cfg = cfg.after(target);
    }

    for dep in before {
        let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
            eprintln!(
                "[bevy_abi_mod] warning: system '{sys_name}' in mod '{mod_name}' has invalid empty `before` target '{dep}'"
            );
            continue;
        };
        let target_gen = if parsed.mod_name == mod_name {
            own_gen
        } else {
            resolve_mod_generation(app, &parsed.mod_name)
        };
        let target = parsed.at_generation(target_gen);
        if !target_exists_in_batch_or_world(app, interned, batch_sets, &target) {
            eprintln!(
                "[bevy_abi_mod] warning: system '{sys_name}' in mod '{mod_name}' declares `before` on unresolved target '{dep}'"
            );
        }
        if target.mod_name == mod_name {
            recorded_sets.push((interned, target.clone()));
        }
        cfg = cfg.before(target);
    }

    app.add_systems(label, cfg);
}

pub(crate) fn schedule_mod_systems(
    app: &mut App,
    mod_id: ModId,
    mod_name: &str,
    systems: Vec<DynamicCSystem>,
) -> Vec<DynamicCSystem> {
    let startup_already_ran = app
        .world()
        .get_resource::<StartupCompleted>()
        .is_some_and(|s| s.has_run());
    let own_gen = resolve_mod_generation(app, mod_name);

    let mut deferred_immediate_startup = Vec::new();
    let mut to_schedule = Vec::with_capacity(systems.len());

    for sys in systems {
        if startup_already_ran && sys.schedule() == ModSchedule::Startup {
            deferred_immediate_startup.push(sys);
        } else {
            to_schedule.push(sys);
        }
    }

    let batch_sets: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = to_schedule
        .iter()
        .map(|s| {
            (
                interned_label_for_mod_schedule(s.schedule()),
                ModNamedSystemSet::with_generation(mod_name, s.system_name(), own_gen),
            )
        })
        .collect();

    let mut recorded_sets = Vec::with_capacity(to_schedule.len());
    for sys in to_schedule {
        match sys.schedule() {
            ModSchedule::Update => add_configured_system_to_label(
                app,
                Update,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
            ModSchedule::Startup => add_configured_system_to_label(
                app,
                Startup,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
            ModSchedule::PreUpdate => add_configured_system_to_label(
                app,
                PreUpdate,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
            ModSchedule::PostUpdate => add_configured_system_to_label(
                app,
                PostUpdate,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
            ModSchedule::FixedUpdate => add_configured_system_to_label(
                app,
                FixedUpdate,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
            ModSchedule::Last => add_configured_system_to_label(
                app,
                Last,
                mod_name,
                own_gen,
                sys,
                &batch_sets,
                &mut recorded_sets,
            ),
        }
    }

    record_mod_named_sets_in_world(app.world_mut(), mod_id, recorded_sets);
    deferred_immediate_startup
}

pub(crate) fn schedule_mod_systems_into_label<S: ScheduleLabel + Clone>(
    app: &mut App,
    schedule: S,
    mod_id: ModId,
    mod_name: &str,
    systems: Vec<DynamicCSystem>,
) {
    let interned = schedule.intern();
    let own_gen = resolve_mod_generation(app, mod_name);
    let batch_sets: HashSet<(InternedScheduleLabel, ModNamedSystemSet)> = systems
        .iter()
        .map(|s| {
            (
                interned,
                ModNamedSystemSet::with_generation(mod_name, s.system_name(), own_gen),
            )
        })
        .collect();

    let mut recorded_sets = Vec::with_capacity(systems.len());
    for sys in systems {
        add_configured_system_to_label(
            app,
            schedule.clone(),
            mod_name,
            own_gen,
            sys,
            &batch_sets,
            &mut recorded_sets,
        );
    }
    record_mod_named_sets_in_world(app.world_mut(), mod_id, recorded_sets);
}

pub(crate) fn run_late_startup_systems_once(
    app: &mut App,
    mod_name: &str,
    startup_systems: Vec<DynamicCSystem>,
) {
    if startup_systems.is_empty() {
        return;
    }

    let own_gen = resolve_mod_generation(app, mod_name);
    let batch_names: HashSet<String> = startup_systems
        .iter()
        .map(|s| s.system_name().to_string())
        .collect();

    let mut temp_schedule = Schedule::default();
    for sys in startup_systems {
        let sys_name = sys.system_name().to_string();
        let own_set = ModNamedSystemSet::with_generation(mod_name, &sys_name, own_gen);
        let after = sys.after_systems().to_vec();
        let before = sys.before_systems().to_vec();

        let mut cfg = sys.in_set(own_set);
        for dep in after {
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                eprintln!(
                    "[bevy_abi_mod] warning: startup system '{sys_name}' in mod '{mod_name}' has invalid empty `after` target '{dep}'"
                );
                continue;
            };
            let target_gen = if parsed.mod_name == mod_name {
                own_gen
            } else {
                resolve_mod_generation(app, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if target.mod_name == mod_name && !batch_names.contains(&target.system_name) {
                eprintln!(
                    "[bevy_abi_mod] warning: startup system '{sys_name}' in mod '{mod_name}' declares `after` on unresolved startup target '{dep}'"
                );
            }
            cfg = cfg.after(target);
        }
        for dep in before {
            let Some(parsed) = ModNamedSystemSet::parse_target(&dep, mod_name) else {
                eprintln!(
                    "[bevy_abi_mod] warning: startup system '{sys_name}' in mod '{mod_name}' has invalid empty `before` target '{dep}'"
                );
                continue;
            };
            let target_gen = if parsed.mod_name == mod_name {
                own_gen
            } else {
                resolve_mod_generation(app, &parsed.mod_name)
            };
            let target = parsed.at_generation(target_gen);
            if target.mod_name == mod_name && !batch_names.contains(&target.system_name) {
                eprintln!(
                    "[bevy_abi_mod] warning: startup system '{sys_name}' in mod '{mod_name}' declares `before` on unresolved startup target '{dep}'"
                );
            }
            cfg = cfg.before(target);
        }
        temp_schedule.add_systems(cfg);
    }

    temp_schedule.run(app.world_mut());
}
