//! Optional `bevy_diagnostic` integration for guest mod CPU time, fault counts, queued commands,
//! and per-mod resource/frame-time budget telemetry.

use crate::budgets::ModBudgetTelemetry;
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

/// Aggregate cumulative budget breaches across all tracked guest systems
/// (`"bevy_abi_mod/total/budget_breach_count"`).
pub const TOTAL_BUDGET_BREACH_COUNT_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/budget_breach_count");

/// Aggregate deferred entity spawns queued across all guest systems per frame
/// (`"bevy_abi_mod/total/spawns_queued"`).
pub const TOTAL_SPAWNS_QUEUED_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/spawns_queued");

/// Maximum observed per-system `CommandArena` payload byte peak across all guest systems
/// (`"bevy_abi_mod/total/arena_bytes_peak"`).
pub const TOTAL_ARENA_BYTES_PEAK_PATH: DiagnosticPath =
    DiagnosticPath::const_new("bevy_abi_mod/total/arena_bytes_peak");

const AGGREGATE_DIAGNOSTICS: &[(DiagnosticPath, &str)] = &[
    (TOTAL_CPU_TIME_US_PATH, " us"),
    (TOTAL_FAULT_COUNT_PATH, ""),
    (TOTAL_COMMANDS_QUEUED_PATH, ""),
    (TOTAL_BUDGET_BREACH_COUNT_PATH, ""),
    (TOTAL_SPAWNS_QUEUED_PATH, ""),
    (TOTAL_ARENA_BYTES_PEAK_PATH, " B"),
];

const PER_MOD_METRICS: &[(&str, &str)] = &[
    ("cpu_time_us", " us"),
    ("fault_count", ""),
    ("commands_queued", ""),
    ("budget_breach_count", ""),
    ("p99_time_us", " us"),
    ("spawns_queued", ""),
    ("arena_bytes_peak", " B"),
];

fn sanitize_diag_segment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len().max(1));
    for ch in raw.chars() {
        out.push(if ch == '/' || ch == '\0' { '_' } else { ch });
    }
    if out.is_empty() {
        out.push_str("unnamed");
    }
    out
}

fn mod_metric_path(mod_name: &str, metric: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/{metric}"))
}

fn ensure_diagnostic(store: &mut DiagnosticsStore, path: DiagnosticPath, suffix: &'static str) {
    if store.get(&path).is_none() {
        let diag = if suffix.is_empty() {
            Diagnostic::new(path)
        } else {
            Diagnostic::new(path).with_suffix(suffix)
        };
        store.add(diag);
    }
}

/// Returns the per-mod CPU time [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/cpu_time_us"`).
pub fn mod_cpu_time_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "cpu_time_us")
}

/// Returns the per-mod fault count [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/fault_count"`).
pub fn mod_fault_count_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "fault_count")
}

/// Returns the per-mod queued commands [`DiagnosticPath`] (`"bevy_abi_mod/mod/<mod_name>/commands_queued"`).
pub fn mod_commands_queued_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "commands_queued")
}

/// Returns the per-mod cumulative budget breach count [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/budget_breach_count"`).
pub fn mod_budget_breach_count_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "budget_breach_count")
}

/// Returns the per-mod peak sliding-window `p99` system duration [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/p99_time_us"`).
pub fn mod_p99_time_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "p99_time_us")
}

/// Returns the per-mod deferred entity spawns queued in the last frame [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/spawns_queued"`).
pub fn mod_spawns_queued_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "spawns_queued")
}

/// Returns the per-mod peak `CommandArena` payload bytes [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/arena_bytes_peak"`).
pub fn mod_arena_bytes_peak_path(mod_name: &str) -> DiagnosticPath {
    mod_metric_path(mod_name, "arena_bytes_peak")
}

/// Ensures [`ModDiagnosticsCollector`], [`DiagnosticsStore`], and aggregate diagnostic entries exist in `world`.
pub(crate) fn ensure_diagnostics_resources(world: &mut World) {
    world.init_resource::<ModDiagnosticsCollector>();
    world.init_resource::<DiagnosticsStore>();
    if let Some(mut store) = world.get_resource_mut::<DiagnosticsStore>() {
        for (path, suffix) in AGGREGATE_DIAGNOSTICS {
            ensure_diagnostic(&mut store, path.clone(), suffix);
        }
    }
}

/// Registers per-mod `Diagnostic` entries (`cpu_time_us`, `fault_count`, `commands_queued`, and
/// budget telemetry paths) in `world`.
pub(crate) fn register_mod_diagnostics_in_world(world: &mut World, mod_name: &str) {
    ensure_diagnostics_resources(world);
    if let Some(mut store) = world.get_resource_mut::<DiagnosticsStore>() {
        for (metric, suffix) in PER_MOD_METRICS {
            ensure_diagnostic(&mut store, mod_metric_path(mod_name, metric), suffix);
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
    ensure_diagnostic(store, path.clone(), suffix);
    if let Some(diag) = store.get_mut(&path)
        && diag.is_enabled
    {
        diag.add_measurement(DiagnosticMeasurement { time: now, value });
    }
}

/// `Last`-schedule Bevy system draining [`ModDiagnosticsCollector`] and [`ModBudgetTelemetry`]
/// into [`DiagnosticsStore`].
pub fn flush_mod_diagnostics(
    collector: Res<ModDiagnosticsCollector>,
    host: Option<Res<ModHost>>,
    budget_telemetry: Option<Res<ModBudgetTelemetry>>,
    mut store: ResMut<DiagnosticsStore>,
) {
    let samples = collector.drain();
    let now = Instant::now();

    let mut per_mod: HashMap<Arc<str>, (f64, usize, usize)> = HashMap::default();
    let mut total_cpu_us = 0.0f64;
    let mut total_cmds = 0usize;

    for sample in samples {
        total_cpu_us += sample.cpu_time_us;
        total_cmds += sample.commands_queued;
        let entry = per_mod.entry(sample.mod_name).or_insert((0.0, 0, 0));
        entry.0 += sample.cpu_time_us;
        entry.1 += sample.commands_queued;
        if sample.faulted {
            entry.2 += 1;
        }
    }

    if let Some(ref mod_host) = host {
        for loaded in mod_host.loaded_mods() {
            let entry = per_mod
                .entry(Arc::from(loaded.name()))
                .or_insert((0.0, 0, 0));
            entry.2 = loaded.fault_count();
        }
    }
    let total_faults: usize = per_mod.values().map(|(_, _, f)| *f).sum();
    let mut record = |path: DiagnosticPath, suffix: &'static str, value: f64| {
        push_measurement(&mut store, path, suffix, now, value);
    };

    record(TOTAL_CPU_TIME_US_PATH, " us", total_cpu_us);
    record(TOTAL_FAULT_COUNT_PATH, "", total_faults as f64);
    record(TOTAL_COMMANDS_QUEUED_PATH, "", total_cmds as f64);

    for (mod_name, (cpu, cmds, faults)) in per_mod {
        record(mod_cpu_time_path(&mod_name), " us", cpu);
        record(mod_commands_queued_path(&mod_name), "", cmds as f64);
        record(mod_fault_count_path(&mod_name), "", faults as f64);
    }

    if let Some(ref telemetry) = budget_telemetry {
        let mut total_breaches = 0u64;
        let mut total_spawns = 0usize;
        let mut total_arena_peak = 0usize;
        let mut per_mod_budget: HashMap<String, (u64, u64, usize, usize)> = HashMap::default();

        for snap in telemetry.system_budget_snapshots() {
            total_breaches = total_breaches.saturating_add(snap.breach_count);
            total_spawns = total_spawns.saturating_add(snap.spawns_last_frame);
            total_arena_peak = total_arena_peak.max(snap.arena_bytes_peak);

            let entry = per_mod_budget.entry(snap.mod_name).or_insert((0, 0, 0, 0));
            entry.0 += snap.breach_count;
            entry.1 = entry.1.max(snap.p99_us);
            entry.2 += snap.spawns_last_frame;
            entry.3 = entry.3.max(snap.arena_bytes_peak);
        }

        record(TOTAL_BUDGET_BREACH_COUNT_PATH, "", total_breaches as f64);
        record(TOTAL_SPAWNS_QUEUED_PATH, "", total_spawns as f64);
        record(TOTAL_ARENA_BYTES_PEAK_PATH, " B", total_arena_peak as f64);

        for (mod_name, (breaches, p99, spawns, arena_peak)) in per_mod_budget {
            record(mod_budget_breach_count_path(&mod_name), "", breaches as f64);
            record(mod_p99_time_path(&mod_name), " us", p99 as f64);
            record(mod_spawns_queued_path(&mod_name), "", spawns as f64);
            record(
                mod_arena_bytes_peak_path(&mod_name),
                " B",
                arena_peak as f64,
            );
        }
    }
}
