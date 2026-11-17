//! Optional `bevy_diagnostic` integration for guest mod CPU time, fault counts, and queued commands.

use bevy_abi_mod_host_sys::{ModDiagnosticsCollector, ModHost};
use bevy_diagnostic::{Diagnostic, DiagnosticMeasurement, DiagnosticPath, DiagnosticsStore};
use bevy_ecs::system::{Res, ResMut};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use bevy_platform::time::Instant;
use std::sync::Arc;

/// Aggregate CPU time across all guest systems per frame (`"bevy_abi_mod/total/cpu_time_us"`).
pub const TOTAL_CPU_TIME_US_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/cpu_time_us");

/// Aggregate cumulative fault count across all active mods (`"bevy_abi_mod/total/fault_count"`).
pub const TOTAL_FAULT_COUNT_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/fault_count");

/// Aggregate deferred commands queued across all guest systems per frame (`"bevy_abi_mod/total/commands_queued"`).
pub const TOTAL_COMMANDS_QUEUED_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/commands_queued");

fn sanitize_diag_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().max(1));
    for ch in raw.chars() {
        if ch == '/' || ch == '\0' {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    if out.is_empty() {
        out.push_str("unnamed");
    }
    out
}

/// Returns the per-mod CPU time [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/cpu_time_us"`).
pub fn mod_cpu_time_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/cpu_time_us"))
}

/// Returns the per-mod fault count [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/fault_count"`).
pub fn mod_fault_count_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/fault_count"))
}

/// Returns the per-mod queued commands [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/commands_queued"`).
pub fn mod_commands_queued_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/commands_queued"))
}

/// Ensures [`ModDiagnosticsCollector`], [`DiagnosticsStore`], and aggregate diagnostic entries exist in `world`.
pub(crate) fn ensure_diagnostics_resources(world: &mut World) {
    if !world.contains_resource::<ModDiagnosticsCollector>() {
        world.insert_resource(ModDiagnosticsCollector::new());
    }
    if !world.contains_resource::<DiagnosticsStore>() {
        world.insert_resource(DiagnosticsStore::default());
    }
    if let Some(mut store) = world.get_resource_mut::<DiagnosticsStore>() {
        if store.get(&TOTAL_CPU_TIME_US_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_CPU_TIME_US_PATH).with_suffix(" us"));
        }
        if store.get(&TOTAL_FAULT_COUNT_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_FAULT_COUNT_PATH));
        }
        if store.get(&TOTAL_COMMANDS_QUEUED_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_COMMANDS_QUEUED_PATH));
        }
    }
}

/// Registers per-mod `Diagnostic` entries (`cpu_time_us`, `fault_count`, `commands_queued`) in `world`.
pub(crate) fn register_mod_diagnostics_in_world(world: &mut World, mod_name: &str) {
    ensure_diagnostics_resources(world);
    let cpu_path = mod_cpu_time_path(mod_name);
    let fault_path = mod_fault_count_path(mod_name);
    let cmd_path = mod_commands_queued_path(mod_name);
    if let Some(mut store) = world.get_resource_mut::<DiagnosticsStore>() {
        if store.get(&cpu_path).is_none() {
            store.add(Diagnostic::new(cpu_path).with_suffix(" us"));
        }
        if store.get(&fault_path).is_none() {
            store.add(Diagnostic::new(fault_path));
        }
        if store.get(&cmd_path).is_none() {
            store.add(Diagnostic::new(cmd_path));
        }
    }
}

fn push_measurement(
    store: &mut DiagnosticsStore,
    path: DiagnosticPath,
    suffix: &'static str,
    now: Instant,
    value: f64,
) {
    if store.get(&path).is_none() {
        store.add(Diagnostic::new(path.clone()).with_suffix(suffix));
    }
    if let Some(diag) = store.get_mut(&path)
        && diag.is_enabled
    {
        diag.add_measurement(DiagnosticMeasurement { time: now, value });
    }
}

/// `Last`-schedule Bevy system draining [`ModDiagnosticsCollector`] into [`DiagnosticsStore`].
pub fn flush_mod_diagnostics(
    collector: Res<ModDiagnosticsCollector>,
    host: Option<Res<ModHost>>,
    mut store: ResMut<DiagnosticsStore>,
) {
    let samples = collector.drain();
    let now = Instant::now();

    let mut per_mod_cpu: HashMap<Arc<str>, f64> = HashMap::default();
    let mut per_mod_cmds: HashMap<Arc<str>, usize> = HashMap::default();
    let mut per_mod_faults: HashMap<Arc<str>, usize> = HashMap::default();

    let mut total_cpu_us = 0.0f64;
    let mut total_cmds = 0usize;

    for sample in samples {
        total_cpu_us += sample.cpu_time_us;
        total_cmds += sample.commands_queued;
        *per_mod_cpu
            .entry(Arc::clone(&sample.mod_name))
            .or_insert(0.0) += sample.cpu_time_us;
        *per_mod_cmds
            .entry(Arc::clone(&sample.mod_name))
            .or_insert(0) += sample.commands_queued;
        if sample.faulted {
            *per_mod_faults.entry(sample.mod_name).or_insert(0) += 1;
        }
    }

    if let Some(ref mod_host) = host {
        for loaded in mod_host.loaded_mods() {
            let name_arc: Arc<str> = Arc::from(loaded.name());
            per_mod_cpu.entry(Arc::clone(&name_arc)).or_insert(0.0);
            per_mod_cmds.entry(Arc::clone(&name_arc)).or_insert(0);
            per_mod_faults.insert(name_arc, loaded.fault_count());
        }
    }
    let total_faults: usize = per_mod_faults.values().sum();

    push_measurement(&mut store, TOTAL_CPU_TIME_US_PATH, " us", now, total_cpu_us);
    push_measurement(
        &mut store,
        TOTAL_FAULT_COUNT_PATH,
        "",
        now,
        total_faults as f64,
    );
    push_measurement(
        &mut store,
        TOTAL_COMMANDS_QUEUED_PATH,
        "",
        now,
        total_cmds as f64,
    );

    for (mod_name, cpu) in per_mod_cpu {
        let cmds = per_mod_cmds.get(&mod_name).copied().unwrap_or(0);
        let faults = per_mod_faults.get(&mod_name).copied().unwrap_or(0);
        push_measurement(&mut store, mod_cpu_time_path(&mod_name), " us", now, cpu);
        push_measurement(
            &mut store,
            mod_commands_queued_path(&mod_name),
            "",
            now,
            cmds as f64,
        );
        push_measurement(
            &mut store,
            mod_fault_count_path(&mod_name),
            "",
            now,
            faults as f64,
        );
    }
}
