//! Per-mod resource quotas, sliding-window frame-time watchdogs, and budget telemetry
//! (`ModBudgetPolicy`, `ModBudgetBreach`, `ModBudgetSnapshot`, `ModBudgetTelemetry`, `ModBudgetExt`).
//!
//! All policy evaluation, sliding-window percentile computation (`p50_us` / `p99_us`), breach
//! tracking, and quarantine coordination live here in `#![forbid(unsafe_code)]` Rust and bridge
//! into `bevy_abi_mod_host_sys`'s `GuestSystem` and `HostCommands` via [`ModBudgetRuntime`].

use bevy_abi_mod_host_sys::{
    BudgetWatchdogAction, CommandQuotaLimits, GuestSystemRunMetrics, ModBudgetHook,
    ModBudgetRuntime, ModId, ModQuarantine,
};
use bevy_app::App;
use bevy_ecs::change_detection::DetectChanges;
use bevy_ecs::message::{Message, Messages};
use bevy_ecs::prelude::Resource;
use bevy_ecs::system::{Res, ResMut};
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, RwLock};

/// Default sliding-window length (`32` system executions) used for `p50_us` / `p99_us` calculation.
pub const DEFAULT_BUDGET_WINDOW_SIZE: usize = 32;

/// Default number of consecutive over-budget `p99_us` evaluations (`3`) before auto-quarantine.
pub const DEFAULT_MAX_CONSECUTIVE_BREACHES: u32 = 3;

/// Hard bound on buffered per-frame [`ModBudgetBreach`] notifications so an adversarial mod
/// cannot exhaust host memory via breach-message flooding.
const MAX_PENDING_BREACHES: usize = 4096;

/// Resource quota and frame-time watchdog limits applied globally or as a per-mod override in
/// [`ModBudgetPolicy`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModBudgetLimits {
    /// Hard cap on per-system `CommandArena` queued payload bytes (`None` = unlimited).
    pub max_arena_bytes_per_system: Option<usize>,
    /// Hard cap on deferred entity spawns queued by a mod within a single frame across
    /// `cmd_spawn`, `cmd_spawn_empty`, `cmd_spawn_persistent`, `cmd_spawn_batch`, and
    /// `cmd_spawn_scene` (`None` = unlimited).
    pub max_spawns_per_frame: Option<usize>,
    /// Maximum allowed sliding-window `p99` system wall-clock execution duration in microseconds
    /// (`None` = watchdog disabled).
    pub max_p99_system_micros: Option<u64>,
    /// Number of recent system execution samples retained in the sliding `p50`/`p99` window
    /// (clamped to `>= 1`).
    pub window_size: usize,
    /// Number of consecutive over-budget `p99` evaluations required before automatically
    /// quarantining the offending system (clamped to `>= 1`).
    pub max_consecutive_breaches: u32,
}

impl Default for ModBudgetLimits {
    #[inline]
    fn default() -> Self {
        Self::unlimited()
    }
}

impl ModBudgetLimits {
    /// Returns an unlimited [`ModBudgetLimits`] profile (all caps `None`, default sliding window).
    #[inline]
    pub const fn unlimited() -> Self {
        Self {
            max_arena_bytes_per_system: None,
            max_spawns_per_frame: None,
            max_p99_system_micros: None,
            window_size: DEFAULT_BUDGET_WINDOW_SIZE,
            max_consecutive_breaches: DEFAULT_MAX_CONSECUTIVE_BREACHES,
        }
    }

    /// Sets the per-system `CommandArena` byte cap.
    #[inline]
    #[must_use]
    pub const fn with_max_arena_bytes_per_system(mut self, bytes: Option<usize>) -> Self {
        self.max_arena_bytes_per_system = bytes;
        self
    }

    /// Sets the per-mod deferred entity spawn cap per frame.
    #[inline]
    #[must_use]
    pub const fn with_max_spawns_per_frame(mut self, spawns: Option<usize>) -> Self {
        self.max_spawns_per_frame = spawns;
        self
    }

    /// Sets the sliding-window `p99` execution time ceiling in microseconds.
    #[inline]
    #[must_use]
    pub const fn with_max_p99_system_micros(mut self, micros: Option<u64>) -> Self {
        self.max_p99_system_micros = micros;
        self
    }

    /// Sets the sliding-window sample count (clamped to `>= 1` at runtime).
    #[inline]
    #[must_use]
    pub const fn with_window_size(mut self, window_size: usize) -> Self {
        self.window_size = window_size;
        self
    }

    /// Sets the consecutive over-budget evaluation threshold before auto-quarantine
    /// (clamped to `>= 1` at runtime).
    #[inline]
    #[must_use]
    pub const fn with_max_consecutive_breaches(mut self, breaches: u32) -> Self {
        self.max_consecutive_breaches = breaches;
        self
    }

    /// Returns the effective sliding-window sample count (`>= 1`).
    #[inline]
    pub const fn effective_window_size(&self) -> usize {
        if self.window_size == 0 {
            1
        } else {
            self.window_size
        }
    }

    /// Returns the effective consecutive breach threshold (`>= 1`).
    #[inline]
    pub const fn effective_max_consecutive_breaches(&self) -> u32 {
        if self.max_consecutive_breaches == 0 {
            1
        } else {
            self.max_consecutive_breaches
        }
    }
}

/// World [`Resource`] configuring global and per-mod resource quotas and frame-time watchdogs.
///
/// All caps default to `None` (unlimited) so existing mods execute without restriction unless the
/// host opts into budgets via [`AbiModPlugin::with_budget_policy`](crate::AbiModPlugin::with_budget_policy),
/// [`ModBudgetExt::set_mod_budget_policy`], or `app.insert_resource(ModBudgetPolicy { .. })`.
#[derive(Resource, Clone, Debug, PartialEq, Eq)]
pub struct ModBudgetPolicy {
    /// Default hard cap on per-system `CommandArena` queued payload bytes (`None` = unlimited).
    pub max_arena_bytes_per_system: Option<usize>,
    /// Default hard cap on deferred entity spawns queued by a mod within a frame (`None` = unlimited).
    pub max_spawns_per_frame: Option<usize>,
    /// Default sliding-window `p99` system duration ceiling in microseconds (`None` = disabled).
    pub max_p99_system_micros: Option<u64>,
    /// Default sliding-window sample count (`>= 1`).
    pub window_size: usize,
    /// Default consecutive over-budget `p99` evaluations before auto-quarantine (`>= 1`).
    pub max_consecutive_breaches: u32,
    /// Per-[`ModId`] budget overrides (highest precedence).
    pub per_mod_overrides: BTreeMap<ModId, ModBudgetLimits>,
    /// Per-mod canonical namespace (`BamManifest::name`) budget overrides (fallback when no
    /// [`ModId`] override matches).
    pub per_name_overrides: BTreeMap<String, ModBudgetLimits>,
}

impl Default for ModBudgetPolicy {
    #[inline]
    fn default() -> Self {
        Self::unlimited()
    }
}

impl ModBudgetPolicy {
    /// Returns an unlimited [`ModBudgetPolicy`] with no per-mod overrides.
    #[inline]
    pub fn unlimited() -> Self {
        Self::from_limits(ModBudgetLimits::unlimited())
    }

    /// Constructs a [`ModBudgetPolicy`] whose global defaults come from `limits`.
    #[inline]
    pub fn from_limits(limits: ModBudgetLimits) -> Self {
        Self {
            max_arena_bytes_per_system: limits.max_arena_bytes_per_system,
            max_spawns_per_frame: limits.max_spawns_per_frame,
            max_p99_system_micros: limits.max_p99_system_micros,
            window_size: limits.window_size,
            max_consecutive_breaches: limits.max_consecutive_breaches,
            per_mod_overrides: BTreeMap::new(),
            per_name_overrides: BTreeMap::new(),
        }
    }

    /// Extracts the global default [`ModBudgetLimits`] configured on this policy.
    #[inline]
    pub const fn default_limits(&self) -> ModBudgetLimits {
        ModBudgetLimits {
            max_arena_bytes_per_system: self.max_arena_bytes_per_system,
            max_spawns_per_frame: self.max_spawns_per_frame,
            max_p99_system_micros: self.max_p99_system_micros,
            window_size: self.window_size,
            max_consecutive_breaches: self.max_consecutive_breaches,
        }
    }

    /// Sets the default per-system `CommandArena` byte cap.
    #[inline]
    #[must_use]
    pub const fn with_max_arena_bytes_per_system(mut self, bytes: Option<usize>) -> Self {
        self.max_arena_bytes_per_system = bytes;
        self
    }

    /// Sets the default per-mod deferred entity spawn cap per frame.
    #[inline]
    #[must_use]
    pub const fn with_max_spawns_per_frame(mut self, spawns: Option<usize>) -> Self {
        self.max_spawns_per_frame = spawns;
        self
    }

    /// Sets the default sliding-window `p99` system duration ceiling in microseconds.
    #[inline]
    #[must_use]
    pub const fn with_max_p99_system_micros(mut self, micros: Option<u64>) -> Self {
        self.max_p99_system_micros = micros;
        self
    }

    /// Sets the default sliding-window sample count.
    #[inline]
    #[must_use]
    pub const fn with_window_size(mut self, window_size: usize) -> Self {
        self.window_size = window_size;
        self
    }

    /// Sets the default consecutive breach threshold before auto-quarantine.
    #[inline]
    #[must_use]
    pub const fn with_max_consecutive_breaches(mut self, breaches: u32) -> Self {
        self.max_consecutive_breaches = breaches;
        self
    }

    /// Attaches a per-[`ModId`] budget override.
    #[must_use]
    pub fn with_mod_override(mut self, mod_id: ModId, limits: ModBudgetLimits) -> Self {
        self.per_mod_overrides.insert(mod_id, limits);
        self
    }

    /// Attaches a per-mod canonical namespace (`BamManifest::name`) budget override.
    #[must_use]
    pub fn with_mod_name_override(
        mut self,
        mod_name: impl Into<String>,
        limits: ModBudgetLimits,
    ) -> Self {
        self.per_name_overrides.insert(mod_name.into(), limits);
        self
    }

    /// Inserts or replaces a per-[`ModId`] budget override in place.
    pub fn set_mod_override(&mut self, mod_id: ModId, limits: ModBudgetLimits) -> &mut Self {
        self.per_mod_overrides.insert(mod_id, limits);
        self
    }

    /// Removes a per-[`ModId`] budget override, returning the prior limits if any.
    pub fn clear_mod_override(&mut self, mod_id: ModId) -> Option<ModBudgetLimits> {
        self.per_mod_overrides.remove(&mod_id)
    }

    /// Inserts or replaces a per-mod canonical namespace (`BamManifest::name`) override in place.
    pub fn set_mod_name_override(
        &mut self,
        mod_name: impl Into<String>,
        limits: ModBudgetLimits,
    ) -> &mut Self {
        self.per_name_overrides.insert(mod_name.into(), limits);
        self
    }

    /// Removes a per-mod canonical namespace override, returning the prior limits if any.
    pub fn clear_mod_name_override(&mut self, mod_name: &str) -> Option<ModBudgetLimits> {
        self.per_name_overrides.remove(mod_name)
    }

    /// Resolves the effective [`ModBudgetLimits`] for `(mod_id, mod_name)`.
    ///
    /// Precedence order:
    /// 1. Explicit `per_mod_overrides` entry for `mod_id`
    /// 2. Explicit `per_name_overrides` entry for `mod_name`
    /// 3. Global defaults configured directly on `self`
    pub fn effective_limits(
        &self,
        mod_id: Option<ModId>,
        mod_name: Option<&str>,
    ) -> ModBudgetLimits {
        if let Some(id) = mod_id
            && let Some(&limits) = self.per_mod_overrides.get(&id)
        {
            return limits;
        }
        if let Some(name) = mod_name
            && let Some(&limits) = self.per_name_overrides.get(name)
        {
            return limits;
        }
        self.default_limits()
    }
}

/// Category of resource or frame-time quota violation reported in [`ModBudgetBreach`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ModBudgetBreachKind {
    /// Per-system `CommandArena` queued payload byte cap (`max_arena_bytes_per_system`) was exceeded.
    ArenaBytesExceeded,
    /// Per-mod frame deferred spawn cap (`max_spawns_per_frame`) was exceeded.
    SpawnsPerFrameExceeded,
    /// Sliding-window `p99` system duration (`max_p99_system_micros`) was chronically exceeded.
    FrameTimeP99Exceeded,
}

/// Buffered Bevy [`Message`] emitted via `Messages<ModBudgetBreach>` when a guest system violates
/// an active [`ModBudgetPolicy`] quota or frame-time watchdog limit.
#[derive(Message, Clone, Debug, PartialEq, Eq)]
pub struct ModBudgetBreach {
    /// Owning [`ModId`], if associated with a loaded or mod-tagged system.
    pub mod_id: Option<ModId>,
    /// Canonical module namespace (`BamManifest::name` or `"unscoped"`).
    pub mod_name: String,
    /// Registered name of the offending guest system.
    pub system_name: String,
    /// Category of budget limit violated.
    pub kind: ModBudgetBreachKind,
    /// Observed value (arena bytes, spawns queued, or sliding-window `p99` microseconds).
    pub observed: u64,
    /// Configured budget ceiling that was exceeded.
    pub limit: u64,
    /// Rejected command operation count (for arena/spawn caps) or consecutive breach streak
    /// (for frame-time watchdogs).
    pub count: u64,
    /// `true` if this breach triggered automatic quarantine via [`ModQuarantine`].
    pub quarantined: bool,
}

/// Point-in-time resource quota and latency telemetry snapshot for one guest ECS system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModBudgetSnapshot {
    /// Owning [`ModId`], if associated with a mod.
    pub mod_id: Option<ModId>,
    /// Canonical module namespace (`BamManifest::name` or `"unscoped"`).
    pub mod_name: String,
    /// Registered name of the guest system.
    pub system_name: String,
    /// Sliding-window 50th-percentile (median) wall-clock duration in microseconds.
    pub p50_us: u64,
    /// Sliding-window 99th-percentile wall-clock duration in microseconds.
    pub p99_us: u64,
    /// Most recent wall-clock execution duration in microseconds.
    pub last_duration_us: u64,
    /// Number of entities matched or processed in the most recent invocation.
    pub matched_entities: u64,
    /// Number of non-empty ECS tables/archetypes matched in the most recent invocation.
    pub matched_tables: u64,
    /// Number of matched tables skipped via `BAM_COMPONENT_FLAG_SUMMARY_TICK` in the most recent run.
    pub skipped_summary_tick_tables: u64,
    /// Number of query chunks yielded in the most recent invocation.
    pub chunks_yielded: usize,
    /// Number of deferred commands accepted in the most recent invocation.
    pub commands_queued: usize,
    /// Peak `CommandArena` payload bytes observed for this system across frames.
    pub arena_bytes_peak: usize,
    /// Number of deferred entity spawns queued by this system in the most recent frame.
    pub spawns_last_frame: usize,
    /// Total cumulative budget breaches (arena cap, spawn cap, and frame-time watchdog) recorded.
    pub breach_count: u64,
    /// Current consecutive over-budget `p99` breach streak.
    pub consecutive_p99_breaches: u32,
    /// `true` if this system is currently quarantined by the frame-time watchdog.
    pub quarantined: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum ModSpawnKey {
    ModId(ModId),
    Namespace(Arc<str>),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct SystemBudgetKey {
    mod_id: Option<ModId>,
    mod_name: Arc<str>,
    system_name: Arc<str>,
}

struct LatencyWindow {
    samples: Vec<u64>,
    scratch: Vec<u64>,
    cursor: usize,
    capacity: usize,
}

impl LatencyWindow {
    fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        Self {
            samples: Vec::with_capacity(cap),
            scratch: Vec::with_capacity(cap),
            cursor: 0,
            capacity: cap,
        }
    }

    fn resize(&mut self, new_capacity: usize) {
        let new_cap = new_capacity.max(1);
        if self.capacity == new_cap {
            return;
        }
        if self.samples.len() == self.capacity && self.cursor > 0 {
            self.samples.rotate_left(self.cursor);
            self.cursor = 0;
        }
        if self.samples.len() > new_cap {
            let drop_count = self.samples.len() - new_cap;
            self.samples.drain(..drop_count);
        }
        self.capacity = new_cap;
        self.cursor = if self.samples.len() >= new_cap {
            0
        } else {
            self.samples.len()
        };
        if self.scratch.capacity() < new_cap {
            self.scratch.reserve(new_cap - self.scratch.capacity());
        }
    }

    fn clear(&mut self) {
        self.samples.clear();
        self.cursor = 0;
    }

    fn record_and_percentiles(&mut self, duration_us: u64) -> (u64, u64) {
        if self.samples.len() < self.capacity {
            self.samples.push(duration_us);
            self.cursor = if self.samples.len() == self.capacity {
                0
            } else {
                self.samples.len()
            };
        } else {
            self.samples[self.cursor] = duration_us;
            self.cursor = (self.cursor + 1) % self.capacity;
        }
        self.compute_percentiles()
    }

    fn compute_percentiles(&mut self) -> (u64, u64) {
        let n = self.samples.len();
        if n == 0 {
            return (0, 0);
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.samples);
        self.scratch.sort_unstable();
        let p50_idx = (n.saturating_mul(50).div_ceil(100))
            .saturating_sub(1)
            .min(n - 1);
        let p99_idx = (n.saturating_mul(99).div_ceil(100))
            .saturating_sub(1)
            .min(n - 1);
        (self.scratch[p50_idx], self.scratch[p99_idx])
    }
}

struct SystemBudgetEntry {
    window: LatencyWindow,
    p50_us: u64,
    p99_us: u64,
    last_duration_us: u64,
    matched_entities: u64,
    matched_tables: u64,
    skipped_summary_tick_tables: u64,
    chunks_yielded: usize,
    commands_queued: usize,
    arena_bytes_peak: usize,
    spawns_this_frame: usize,
    spawns_last_frame: usize,
    breach_count: u64,
    consecutive_p99_breaches: u32,
    quarantined: bool,
}

impl SystemBudgetEntry {
    fn new(window_size: usize) -> Self {
        Self {
            window: LatencyWindow::new(window_size),
            p50_us: 0,
            p99_us: 0,
            last_duration_us: 0,
            matched_entities: 0,
            matched_tables: 0,
            skipped_summary_tick_tables: 0,
            chunks_yielded: 0,
            commands_queued: 0,
            arena_bytes_peak: 0,
            spawns_this_frame: 0,
            spawns_last_frame: 0,
            breach_count: 0,
            consecutive_p99_breaches: 0,
            quarantined: false,
        }
    }

    fn to_snapshot(&self, key: &SystemBudgetKey) -> ModBudgetSnapshot {
        ModBudgetSnapshot {
            mod_id: key.mod_id,
            mod_name: key.mod_name.to_string(),
            system_name: key.system_name.to_string(),
            p50_us: self.p50_us,
            p99_us: self.p99_us,
            last_duration_us: self.last_duration_us,
            matched_entities: self.matched_entities,
            matched_tables: self.matched_tables,
            skipped_summary_tick_tables: self.skipped_summary_tick_tables,
            chunks_yielded: self.chunks_yielded,
            commands_queued: self.commands_queued,
            arena_bytes_peak: self.arena_bytes_peak,
            spawns_last_frame: self.spawns_last_frame,
            breach_count: self.breach_count,
            consecutive_p99_breaches: self.consecutive_p99_breaches,
            quarantined: self.quarantined,
        }
    }
}

struct BudgetRuntimeState {
    policy: RwLock<ModBudgetPolicy>,
    spawn_counters: RwLock<HashMap<ModSpawnKey, Arc<AtomicUsize>>>,
    systems: Mutex<HashMap<SystemBudgetKey, SystemBudgetEntry>>,
    pending_breaches: Mutex<Vec<ModBudgetBreach>>,
}

impl BudgetRuntimeState {
    fn new(initial_policy: ModBudgetPolicy) -> Self {
        Self {
            policy: RwLock::new(initial_policy),
            spawn_counters: RwLock::new(HashMap::default()),
            systems: Mutex::new(HashMap::default()),
            pending_breaches: Mutex::new(Vec::new()),
        }
    }

    fn sync_policy(&self, policy: &ModBudgetPolicy) {
        let mut guard = self.policy.write().unwrap_or_else(|e| e.into_inner());
        *guard = policy.clone();
    }

    fn effective_limits(&self, mod_id: Option<ModId>, mod_name: &str) -> ModBudgetLimits {
        let guard = self.policy.read().unwrap_or_else(|e| e.into_inner());
        guard.effective_limits(mod_id, Some(mod_name))
    }

    fn get_or_create_spawn_counter(
        &self,
        mod_id: Option<ModId>,
        mod_name: &str,
    ) -> Arc<AtomicUsize> {
        let key = match mod_id {
            Some(id) => ModSpawnKey::ModId(id),
            None => ModSpawnKey::Namespace(Arc::from(mod_name)),
        };
        if let Some(existing) = self
            .spawn_counters
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Arc::clone(existing);
        }
        let mut guard = self
            .spawn_counters
            .write()
            .unwrap_or_else(|e| e.into_inner());
        Arc::clone(
            guard
                .entry(key)
                .or_insert_with(|| Arc::new(AtomicUsize::new(0))),
        )
    }

    fn push_breaches(&self, breaches: [Option<ModBudgetBreach>; 3]) {
        if breaches.iter().all(Option::is_none) {
            return;
        }
        let mut guard = self
            .pending_breaches
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for breach in breaches.into_iter().flatten() {
            if guard.len() < MAX_PENDING_BREACHES {
                guard.push(breach);
            }
        }
    }

    fn drain_pending_breaches(&self) -> Vec<ModBudgetBreach> {
        let mut guard = self
            .pending_breaches
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        core::mem::take(&mut *guard)
    }

    fn end_frame(&self, quarantine: Option<&ModQuarantine>) {
        {
            let mut systems = self.systems.lock().unwrap_or_else(|e| e.into_inner());
            for (key, entry) in systems.iter_mut() {
                entry.spawns_last_frame = entry.spawns_this_frame;
                entry.spawns_this_frame = 0;
                if entry.quarantined
                    && let Some(q) = quarantine
                    && let Some(mod_id) = key.mod_id
                    && !q.is_quarantined(mod_id, &key.system_name)
                {
                    entry.quarantined = false;
                    entry.window.clear();
                    entry.consecutive_p99_breaches = 0;
                }
            }
        }
        let counters = self
            .spawn_counters
            .read()
            .unwrap_or_else(|e| e.into_inner());
        for counter in counters.values() {
            counter.store(0, Ordering::Release);
        }
    }

    fn cleanup_mod(&self, mod_id: ModId) {
        self.spawn_counters
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&ModSpawnKey::ModId(mod_id));
        self.systems
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|key, _| key.mod_id != Some(mod_id));
    }

    fn snapshots(&self) -> Vec<ModBudgetSnapshot> {
        let guard = self.systems.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ModBudgetSnapshot> = guard
            .iter()
            .map(|(key, entry)| entry.to_snapshot(key))
            .collect();
        out.sort_by(|a, b| {
            a.mod_id
                .cmp(&b.mod_id)
                .then_with(|| a.mod_name.cmp(&b.mod_name))
                .then_with(|| a.system_name.cmp(&b.system_name))
        });
        out
    }

    fn snapshot_for(&self, mod_id: Option<ModId>, system_name: &str) -> Option<ModBudgetSnapshot> {
        let guard = self.systems.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .find(|(key, _)| key.mod_id == mod_id && key.system_name.as_ref() == system_name)
            .map(|(key, entry)| entry.to_snapshot(key))
    }

    fn mod_snapshots(&self, mod_id: ModId) -> Vec<ModBudgetSnapshot> {
        let guard = self.systems.lock().unwrap_or_else(|e| e.into_inner());
        let mut out: Vec<ModBudgetSnapshot> = guard
            .iter()
            .filter(|(key, _)| key.mod_id == Some(mod_id))
            .map(|(key, entry)| entry.to_snapshot(key))
            .collect();
        out.sort_by(|a, b| a.system_name.cmp(&b.system_name));
        out
    }

    fn mod_spawns_last_frame(&self, mod_id: ModId) -> usize {
        let guard = self.systems.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .iter()
            .filter(|(key, _)| key.mod_id == Some(mod_id))
            .map(|(_, entry)| entry.spawns_last_frame)
            .sum()
    }

    fn total_breach_count(&self) -> u64 {
        let guard = self.systems.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .values()
            .fold(0u64, |acc, e| acc.saturating_add(e.breach_count))
    }

    fn system_count(&self) -> usize {
        self.systems.lock().unwrap_or_else(|e| e.into_inner()).len()
    }
}

impl ModBudgetHook for BudgetRuntimeState {
    fn prepare_system_quotas(
        &self,
        mod_id: Option<ModId>,
        mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
        let limits = self.effective_limits(mod_id, mod_name);
        let counter = self.get_or_create_spawn_counter(mod_id, mod_name);
        (
            CommandQuotaLimits {
                max_arena_bytes: limits.max_arena_bytes_per_system,
                max_spawns_per_frame: limits.max_spawns_per_frame,
            },
            Some(counter),
        )
    }

    fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
        let limits = self.effective_limits(metrics.mod_id, &metrics.mod_name);
        let window_size = limits.effective_window_size();
        let max_consecutive = limits.effective_max_consecutive_breaches();

        let key = SystemBudgetKey {
            mod_id: metrics.mod_id,
            mod_name: Arc::clone(&metrics.mod_name),
            system_name: Arc::clone(&metrics.system_name),
        };

        let mut pending: [Option<ModBudgetBreach>; 3] = [None, None, None];
        let mut action = BudgetWatchdogAction::default();

        {
            let mut systems = self.systems.lock().unwrap_or_else(|e| e.into_inner());
            let entry = systems
                .entry(key)
                .or_insert_with(|| SystemBudgetEntry::new(window_size));

            if entry.quarantined {
                // System was re-enabled after a prior watchdog quarantine; start a fresh latency
                // sliding window so historical pre-quarantine samples do not trigger an immediate
                // spurious re-quarantine on the first healthy post-recovery frame.
                entry.quarantined = false;
                entry.window.clear();
                entry.consecutive_p99_breaches = 0;
            }
            entry.window.resize(window_size);

            let (p50_us, p99_us) = entry.window.record_and_percentiles(metrics.duration_us);
            entry.p50_us = p50_us;
            entry.p99_us = p99_us;
            entry.last_duration_us = metrics.duration_us;
            entry.matched_entities = metrics.matched_entities;
            entry.matched_tables = metrics.matched_tables;
            entry.skipped_summary_tick_tables = metrics.skipped_summary_tick_tables;
            entry.chunks_yielded = metrics.chunks_yielded;
            entry.commands_queued = metrics.commands_queued;
            entry.arena_bytes_peak = entry.arena_bytes_peak.max(metrics.arena_bytes_used);
            entry.spawns_this_frame = entry
                .spawns_this_frame
                .saturating_add(metrics.spawns_queued);
            entry.spawns_last_frame = entry.spawns_this_frame;

            if metrics.arena_breaches > 0 {
                entry.breach_count = entry
                    .breach_count
                    .saturating_add(metrics.arena_breaches as u64);
                let limit_bytes = limits.max_arena_bytes_per_system.unwrap_or(0) as u64;
                tracing::warn!(
                    system = %metrics.system_name,
                    mod_id = ?metrics.mod_id,
                    mod_name = %metrics.mod_name,
                    arena_bytes_used = metrics.arena_bytes_used,
                    limit_bytes,
                    rejected_ops = metrics.arena_breaches,
                    "guest mod system exceeded max_arena_bytes_per_system budget"
                );
                pending[0] = Some(ModBudgetBreach {
                    mod_id: metrics.mod_id,
                    mod_name: metrics.mod_name.to_string(),
                    system_name: metrics.system_name.to_string(),
                    kind: ModBudgetBreachKind::ArenaBytesExceeded,
                    observed: metrics.arena_bytes_used as u64,
                    limit: limit_bytes,
                    count: metrics.arena_breaches as u64,
                    quarantined: false,
                });
            }

            if metrics.spawn_breaches > 0 {
                entry.breach_count = entry
                    .breach_count
                    .saturating_add(metrics.spawn_breaches as u64);
                let limit_spawns = limits.max_spawns_per_frame.unwrap_or(0) as u64;
                tracing::warn!(
                    system = %metrics.system_name,
                    mod_id = ?metrics.mod_id,
                    mod_name = %metrics.mod_name,
                    spawns_this_frame = entry.spawns_this_frame,
                    limit_spawns,
                    rejected_ops = metrics.spawn_breaches,
                    "guest mod system exceeded max_spawns_per_frame budget"
                );
                pending[1] = Some(ModBudgetBreach {
                    mod_id: metrics.mod_id,
                    mod_name: metrics.mod_name.to_string(),
                    system_name: metrics.system_name.to_string(),
                    kind: ModBudgetBreachKind::SpawnsPerFrameExceeded,
                    observed: entry.spawns_this_frame as u64,
                    limit: limit_spawns,
                    count: metrics.spawn_breaches as u64,
                    quarantined: false,
                });
            }

            if let Some(limit_us) = limits.max_p99_system_micros {
                if p99_us > limit_us {
                    entry.consecutive_p99_breaches =
                        entry.consecutive_p99_breaches.saturating_add(1);
                    if !metrics.faulted && entry.consecutive_p99_breaches >= max_consecutive {
                        entry.breach_count = entry.breach_count.saturating_add(1);
                        entry.quarantined = true;
                        let reason = format!(
                            "guest system '{}' exceeded frame-time budget (p99={}us > limit={}us for {} consecutive runs)",
                            metrics.system_name, p99_us, limit_us, entry.consecutive_p99_breaches
                        );
                        pending[2] = Some(ModBudgetBreach {
                            mod_id: metrics.mod_id,
                            mod_name: metrics.mod_name.to_string(),
                            system_name: metrics.system_name.to_string(),
                            kind: ModBudgetBreachKind::FrameTimeP99Exceeded,
                            observed: p99_us,
                            limit: limit_us,
                            count: u64::from(entry.consecutive_p99_breaches),
                            quarantined: true,
                        });
                        action = BudgetWatchdogAction {
                            quarantine: true,
                            reason: Some(reason),
                        };
                    }
                } else {
                    entry.consecutive_p99_breaches = 0;
                }
            } else {
                entry.consecutive_p99_breaches = 0;
            }
        }

        self.push_breaches(pending);
        action
    }
}

/// World [`Resource`] exposing live per-system latency percentiles, query statistics, command
/// arena peaks, spawn counts, and breach counters collected by [`ModBudgetPolicy`].
///
/// Downstream observability plugins (such as `ModTelemetryPlugin` in `bevy_abi_mod_reflect`) can
/// read [`Self::system_budget_snapshots`] without touching `unsafe` internals.
#[derive(Resource, Clone)]
pub struct ModBudgetTelemetry {
    state: Arc<BudgetRuntimeState>,
}

impl ModBudgetTelemetry {
    fn from_state(state: Arc<BudgetRuntimeState>) -> Self {
        Self { state }
    }

    pub(crate) fn sync_policy(&self, policy: &ModBudgetPolicy) {
        self.state.sync_policy(policy);
    }

    pub(crate) fn drain_pending_breaches(&self) -> Vec<ModBudgetBreach> {
        self.state.drain_pending_breaches()
    }

    pub(crate) fn end_frame(&self, quarantine: Option<&ModQuarantine>) {
        self.state.end_frame(quarantine);
    }

    pub(crate) fn cleanup_mod(&self, mod_id: ModId) {
        self.state.cleanup_mod(mod_id);
    }

    /// Returns a deterministically sorted snapshot of every tracked guest system's budget and
    /// execution metrics.
    pub fn system_budget_snapshots(&self) -> Vec<ModBudgetSnapshot> {
        self.state.snapshots()
    }

    /// Returns the latest [`ModBudgetSnapshot`] for `(mod_id, system_name)`, if recorded.
    pub fn system_snapshot(
        &self,
        mod_id: Option<ModId>,
        system_name: &str,
    ) -> Option<ModBudgetSnapshot> {
        self.state.snapshot_for(mod_id, system_name)
    }

    /// Returns snapshots for all tracked guest systems belonging to `mod_id`.
    pub fn mod_snapshots(&self, mod_id: ModId) -> Vec<ModBudgetSnapshot> {
        self.state.mod_snapshots(mod_id)
    }

    /// Returns the aggregate deferred entity spawns queued by `mod_id` in the most recent frame.
    pub fn mod_spawns_last_frame(&self, mod_id: ModId) -> usize {
        self.state.mod_spawns_last_frame(mod_id)
    }

    /// Returns the cumulative count of budget breaches across all tracked guest systems.
    pub fn total_breach_count(&self) -> u64 {
        self.state.total_breach_count()
    }

    /// Returns the number of tracked guest systems.
    pub fn len(&self) -> usize {
        self.state.system_count()
    }

    /// Returns `true` if no guest system executions have been recorded yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl fmt::Debug for ModBudgetTelemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModBudgetTelemetry")
            .field("tracked_systems", &self.len())
            .field("total_breaches", &self.total_breach_count())
            .finish()
    }
}

/// Marker resource preventing duplicate installation of budget maintenance schedules when
/// `AbiModPlugin` is built.
#[derive(Resource, Default, Debug)]
pub(crate) struct BudgetSystemsInstalled;

/// Ensures [`ModBudgetPolicy`], [`ModBudgetRuntime`], [`ModBudgetTelemetry`], and
/// `Messages<ModBudgetBreach>` exist and are synchronized inside `world`.
pub(crate) fn ensure_budget_resources(world: &mut World) {
    if !world.contains_resource::<ModBudgetPolicy>() {
        world.insert_resource(ModBudgetPolicy::default());
    }
    let policy_snapshot = world.resource::<ModBudgetPolicy>().clone();

    if !world.contains_resource::<ModBudgetTelemetry>()
        || !world.contains_resource::<ModBudgetRuntime>()
    {
        let state = Arc::new(BudgetRuntimeState::new(policy_snapshot));
        world.insert_resource(ModBudgetRuntime::new(
            Arc::clone(&state) as Arc<dyn ModBudgetHook>
        ));
        world.insert_resource(ModBudgetTelemetry::from_state(state));
    } else if let Some(telemetry) = world.get_resource::<ModBudgetTelemetry>() {
        telemetry.sync_policy(&policy_snapshot);
    }

    if !world.contains_resource::<Messages<ModBudgetBreach>>() {
        world.init_resource::<Messages<ModBudgetBreach>>();
    }
}

/// Mod-unload cleanup hook removing stale per-`ModId` runtime counters and telemetry entries when
/// `mod_id` unloads or reloads.
pub(crate) fn cleanup_mod_budgets_in_world(world: &mut World, mod_id: ModId) {
    if let Some(telemetry) = world.get_resource::<ModBudgetTelemetry>() {
        telemetry.cleanup_mod(mod_id);
    }
}

/// `First`-schedule Bevy system synchronizing mutations to [`ModBudgetPolicy`] into
/// [`ModBudgetRuntime`] before any guest systems execute in the current frame.
pub fn sync_mod_budget_policy(policy: Res<ModBudgetPolicy>, telemetry: Res<ModBudgetTelemetry>) {
    if policy.is_changed() {
        telemetry.sync_policy(&policy);
    }
}

/// `Last`-schedule Bevy system draining buffered [`ModBudgetBreach`] notifications into
/// `Messages<ModBudgetBreach>`, synchronizing post-reenable [`ModQuarantine`] state, and
/// resetting per-mod frame spawn counters for the upcoming frame.
pub fn flush_mod_budgets(
    telemetry: Res<ModBudgetTelemetry>,
    quarantine: Option<Res<ModQuarantine>>,
    mut breaches: ResMut<Messages<ModBudgetBreach>>,
) {
    for breach in telemetry.drain_pending_breaches() {
        breaches.write(breach);
    }
    telemetry.end_frame(quarantine.as_deref());
}

/// Ergonomic extension trait on Bevy [`App`] and [`World`] for configuring [`ModBudgetPolicy`]
/// and querying [`ModBudgetTelemetry`] snapshots.
pub trait ModBudgetExt {
    /// Installs or replaces the active [`ModBudgetPolicy`] and immediately synchronizes runtime
    /// quota enforcement.
    fn set_mod_budget_policy(&mut self, policy: ModBudgetPolicy) -> &mut Self;

    /// Installs or replaces a per-[`ModId`] [`ModBudgetLimits`] override on the active
    /// [`ModBudgetPolicy`] and immediately synchronizes runtime enforcement.
    fn set_mod_budget_override(&mut self, mod_id: ModId, limits: ModBudgetLimits) -> &mut Self;

    /// Installs or replaces a per-mod canonical namespace (`BamManifest::name`)
    /// [`ModBudgetLimits`] override on the active [`ModBudgetPolicy`] and immediately
    /// synchronizes runtime enforcement.
    fn set_mod_name_budget_override(
        &mut self,
        mod_name: impl Into<String>,
        limits: ModBudgetLimits,
    ) -> &mut Self;

    /// Returns a deterministically sorted snapshot of all tracked guest systems' budget telemetry.
    fn system_budget_snapshots(&self) -> Vec<ModBudgetSnapshot>;
}

impl ModBudgetExt for World {
    fn set_mod_budget_policy(&mut self, policy: ModBudgetPolicy) -> &mut Self {
        ensure_budget_resources(self);
        if let Some(telemetry) = self.get_resource::<ModBudgetTelemetry>() {
            telemetry.sync_policy(&policy);
        }
        self.insert_resource(policy);
        self
    }

    fn set_mod_budget_override(&mut self, mod_id: ModId, limits: ModBudgetLimits) -> &mut Self {
        ensure_budget_resources(self);
        let updated = {
            let mut policy = self.resource_mut::<ModBudgetPolicy>();
            policy.set_mod_override(mod_id, limits);
            policy.clone()
        };
        if let Some(telemetry) = self.get_resource::<ModBudgetTelemetry>() {
            telemetry.sync_policy(&updated);
        }
        self
    }

    fn set_mod_name_budget_override(
        &mut self,
        mod_name: impl Into<String>,
        limits: ModBudgetLimits,
    ) -> &mut Self {
        ensure_budget_resources(self);
        let updated = {
            let mut policy = self.resource_mut::<ModBudgetPolicy>();
            policy.set_mod_name_override(mod_name, limits);
            policy.clone()
        };
        if let Some(telemetry) = self.get_resource::<ModBudgetTelemetry>() {
            telemetry.sync_policy(&updated);
        }
        self
    }

    fn system_budget_snapshots(&self) -> Vec<ModBudgetSnapshot> {
        self.get_resource::<ModBudgetTelemetry>()
            .map(ModBudgetTelemetry::system_budget_snapshots)
            .unwrap_or_default()
    }
}

impl ModBudgetExt for App {
    #[inline]
    fn set_mod_budget_policy(&mut self, policy: ModBudgetPolicy) -> &mut Self {
        self.world_mut().set_mod_budget_policy(policy);
        self
    }

    #[inline]
    fn set_mod_budget_override(&mut self, mod_id: ModId, limits: ModBudgetLimits) -> &mut Self {
        self.world_mut().set_mod_budget_override(mod_id, limits);
        self
    }

    #[inline]
    fn set_mod_name_budget_override(
        &mut self,
        mod_name: impl Into<String>,
        limits: ModBudgetLimits,
    ) -> &mut Self {
        self.world_mut()
            .set_mod_name_budget_override(mod_name, limits);
        self
    }

    #[inline]
    fn system_budget_snapshots(&self) -> Vec<ModBudgetSnapshot> {
        self.world().system_budget_snapshots()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_window_percentiles_and_resize_preserve_recent_samples() {
        let mut w = LatencyWindow::new(4);
        assert_eq!(w.compute_percentiles(), (0, 0));

        assert_eq!(w.record_and_percentiles(100), (100, 100));
        assert_eq!(w.record_and_percentiles(200), (100, 200));
        assert_eq!(w.record_and_percentiles(300), (200, 300));
        assert_eq!(w.record_and_percentiles(400), (200, 400));
        // Overwrites oldest (100) -> chronological [200, 300, 400, 500]
        assert_eq!(w.record_and_percentiles(500), (300, 500));

        // Shrink window from 4 -> 2: retains two most recent samples [400, 500].
        w.resize(2);
        assert_eq!(w.compute_percentiles(), (400, 500));

        // Grow window from 2 -> 5 and add 50 -> [400, 500, 50] sorted [50, 400, 500].
        w.resize(5);
        assert_eq!(w.record_and_percentiles(50), (400, 500));

        // Zero-clamped window size still works cleanly.
        w.resize(0);
        assert_eq!(w.compute_percentiles(), (50, 50));
    }

    #[test]
    fn policy_override_precedence_prefers_mod_id_then_name_then_defaults() {
        let mut policy = ModBudgetPolicy::default()
            .with_max_arena_bytes_per_system(Some(4096))
            .with_max_spawns_per_frame(Some(100))
            .with_max_p99_system_micros(Some(2000))
            .with_window_size(16)
            .with_max_consecutive_breaches(4)
            .with_mod_name_override(
                "alpha",
                ModBudgetLimits::default().with_max_spawns_per_frame(Some(25)),
            )
            .with_mod_override(
                ModId(7),
                ModBudgetLimits::default().with_max_spawns_per_frame(Some(5)),
            );

        assert_eq!(
            policy
                .effective_limits(Some(ModId(7)), Some("alpha"))
                .max_spawns_per_frame,
            Some(5)
        );
        assert_eq!(
            policy
                .effective_limits(Some(ModId(8)), Some("alpha"))
                .max_spawns_per_frame,
            Some(25)
        );
        assert_eq!(
            policy
                .effective_limits(Some(ModId(8)), Some("beta"))
                .max_spawns_per_frame,
            Some(100)
        );

        assert!(policy.clear_mod_override(ModId(7)).is_some());
        assert_eq!(
            policy
                .effective_limits(Some(ModId(7)), Some("alpha"))
                .max_spawns_per_frame,
            Some(25)
        );
        assert!(policy.clear_mod_name_override("alpha").is_some());
        assert_eq!(
            policy
                .effective_limits(Some(ModId(7)), Some("alpha"))
                .max_spawns_per_frame,
            Some(100)
        );
    }

    #[test]
    fn budget_limits_zero_window_and_breaches_clamp_to_one() {
        let lim = ModBudgetLimits::unlimited()
            .with_window_size(0)
            .with_max_consecutive_breaches(0);
        assert_eq!(lim.effective_window_size(), 1);
        assert_eq!(lim.effective_max_consecutive_breaches(), 1);
    }
}
