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

/// Returns the per-mod cumulative budget breach count [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/budget_breach_count"`).
pub fn mod_budget_breach_count_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/budget_breach_count"))
}

/// Returns the per-mod peak sliding-window `p99` system duration [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/p99_time_us"`).
pub fn mod_p99_time_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/p99_time_us"))
}

/// Returns the per-mod deferred entity spawns queued in the last frame [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/spawns_queued"`).
pub fn mod_spawns_queued_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/spawns_queued"))
}

/// Returns the per-mod peak `CommandArena` payload bytes [`DiagnosticPath`]
/// (`"bevy_abi_mod/mod/<mod_name>/arena_bytes_peak"`).
pub fn mod_arena_bytes_peak_path(mod_name: &str) -> DiagnosticPath {
    let seg = sanitize_diag_segment(mod_name);
    DiagnosticPath::new(format!("bevy_abi_mod/mod/{seg}/arena_bytes_peak"))
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
        if store.get(&TOTAL_BUDGET_BREACH_COUNT_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_BUDGET_BREACH_COUNT_PATH));
        }
        if store.get(&TOTAL_SPAWNS_QUEUED_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_SPAWNS_QUEUED_PATH));
        }
        if store.get(&TOTAL_ARENA_BYTES_PEAK_PATH).is_none() {
            store.add(Diagnostic::new(TOTAL_ARENA_BYTES_PEAK_PATH).with_suffix(" B"));
        }
    }
}

/// Registers per-mod `Diagnostic` entries (`cpu_time_us`, `fault_count`, `commands_queued`, and
/// budget telemetry paths) in `world`.
pub(crate) fn register_mod_diagnostics_in_world(world: &mut World, mod_name: &str) {
    ensure_diagnostics_resources(world);
    let cpu_path = mod_cpu_time_path(mod_name);
    let fault_path = mod_fault_count_path(mod_name);
    let cmd_path = mod_commands_queued_path(mod_name);
    let breach_path = mod_budget_breach_count_path(mod_name);
    let p99_path = mod_p99_time_path(mod_name);
    let spawns_path = mod_spawns_queued_path(mod_name);
    let arena_path = mod_arena_bytes_peak_path(mod_name);
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
        if store.get(&breach_path).is_none() {
            store.add(Diagnostic::new(breach_path));
        }
        if store.get(&p99_path).is_none() {
            store.add(Diagnostic::new(p99_path).with_suffix(" us"));
        }
        if store.get(&spawns_path).is_none() {
            store.add(Diagnostic::new(spawns_path));
        }
        if store.get(&arena_path).is_none() {
            store.add(Diagnostic::new(arena_path).with_suffix(" B"));
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

    if let Some(ref telemetry) = budget_telemetry {
        let snapshots = telemetry.system_budget_snapshots();
        let mut total_breaches = 0u64;
        let mut total_spawns = 0usize;
        let mut total_arena_peak = 0usize;
        let mut per_mod_breaches: HashMap<String, u64> = HashMap::default();
        let mut per_mod_p99: HashMap<String, u64> = HashMap::default();
        let mut per_mod_spawns: HashMap<String, usize> = HashMap::default();
        let mut per_mod_arena_peak: HashMap<String, usize> = HashMap::default();

        for snap in snapshots {
            total_breaches = total_breaches.saturating_add(snap.breach_count);
            total_spawns = total_spawns.saturating_add(snap.spawns_last_frame);
            total_arena_peak = total_arena_peak.max(snap.arena_bytes_peak);

            *per_mod_breaches.entry(snap.mod_name.clone()).or_insert(0) += snap.breach_count;
            let p99_entry = per_mod_p99.entry(snap.mod_name.clone()).or_insert(0);
            *p99_entry = (*p99_entry).max(snap.p99_us);
            *per_mod_spawns.entry(snap.mod_name.clone()).or_insert(0) += snap.spawns_last_frame;
            let arena_entry = per_mod_arena_peak.entry(snap.mod_name).or_insert(0);
            *arena_entry = (*arena_entry).max(snap.arena_bytes_peak);
        }

        push_measurement(
            &mut store,
            TOTAL_BUDGET_BREACH_COUNT_PATH,
            "",
            now,
            total_breaches as f64,
        );
        push_measurement(
            &mut store,
            TOTAL_SPAWNS_QUEUED_PATH,
            "",
            now,
            total_spawns as f64,
        );
        push_measurement(
            &mut store,
            TOTAL_ARENA_BYTES_PEAK_PATH,
            " B",
            now,
            total_arena_peak as f64,
        );

        for (mod_name, breaches) in per_mod_breaches {
            let p99 = per_mod_p99.get(&mod_name).copied().unwrap_or(0);
            let spawns = per_mod_spawns.get(&mod_name).copied().unwrap_or(0);
            let arena_peak = per_mod_arena_peak.get(&mod_name).copied().unwrap_or(0);
            push_measurement(
                &mut store,
                mod_budget_breach_count_path(&mod_name),
                "",
                now,
                breaches as f64,
            );
            push_measurement(
                &mut store,
                mod_p99_time_path(&mod_name),
                " us",
                now,
                p99 as f64,
            );
            push_measurement(
                &mut store,
                mod_spawns_queued_path(&mod_name),
                "",
                now,
                spawns as f64,
            );
            push_measurement(
                &mut store,
                mod_arena_bytes_peak_path(&mod_name),
                " B",
                now,
                arena_peak as f64,
            );
        }
    }
}
