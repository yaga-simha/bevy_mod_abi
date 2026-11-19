//! Fault policies, `ModQuarantine` resource, runtime system errors, and initialization diagnostics.

use crate::ffi::commands::CommandQuotaLimits;
use crate::loader::ModId;
use bevy_abi_mod_sys::{BamHostCtx, FfiResult};
use bevy_ecs::change_detection::Tick;
use bevy_ecs::error::{BevyError, ContextExt, ErrorContext, Severity};
use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use bevy_ecs::utils::DebugName;
use bevy_ecs::world::World;
use bevy_platform::collections::HashMap;
use core::cell::RefCell;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use thiserror::Error;

/// Error-handling policy governing how [`crate::GuestSystem`] responds when a guest callback
/// returns an error [`FfiResult`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModErrorPolicy {
    /// Quarantines the faulting guest system (`enabled = false`), logs via `tracing::warn!`,
    /// and returns `Ok(())` (default).
    #[default]
    Quarantine,
    /// Quarantines the faulting guest system, logs via `tracing::error!`, and returns `Ok(())`.
    Log,
    /// Quarantines the faulting guest system, logs via `tracing::error!`, and propagates
    /// [`ModSystemError`] via `Err(RunSystemError::from(...))` to Bevy's error handler.
    ReportToBevy,
    /// Discards uncommitted commands, emits [`ModSystemFaulted`], and delegates fault handling
    /// directly to Bevy's world [`FallbackErrorHandler`](bevy_ecs::error::FallbackErrorHandler)
    /// with a contextualized [`BevyError`] (`.with_context(...)`)
    /// without auto-quarantining the system.
    DelegateToBevyFallback,
}

/// World resource configuring the default [`ModErrorPolicy`] for dynamic guest systems.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModErrorPolicyConfig(pub ModErrorPolicy);

/// World resource tracking per-system quarantine flags so host code can inspect and re-enable
/// quarantined guest systems without reloading the entire mod.
#[derive(Resource, Default, Clone, Debug)]
pub struct ModQuarantine {
    systems: HashMap<(ModId, Arc<str>), Arc<AtomicBool>>,
}

impl ModQuarantine {
    /// Creates an empty [`ModQuarantine`] registry.
    #[inline]
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a scheduled [`crate::GuestSystem`]'s enablement flag under `(mod_id, system_name)`.
    pub fn register_system(
        &mut self,
        mod_id: ModId,
        system_name: Arc<str>,
        enabled_flag: Arc<AtomicBool>,
    ) {
        self.systems.insert((mod_id, system_name), enabled_flag);
    }

    /// Removes all tracked systems belonging to `mod_id` when `mod_id` unloads.
    pub fn unregister_mod(&mut self, mod_id: ModId) {
        self.systems.retain(|(id, _), _| *id != mod_id);
    }

    /// Returns `true` if `(mod_id, system_name)` is currently quarantined (disabled).
    #[inline]
    pub fn is_quarantined(&self, mod_id: ModId, system_name: &str) -> bool {
        self.systems
            .get(&(mod_id, Arc::from(system_name)))
            .is_some_and(|flag| !flag.load(Ordering::Acquire))
    }

    /// Re-enables a quarantined guest system identified by `(mod_id, system_name)`.
    ///
    /// Returns `true` if the system existed and transitioned from quarantined (`false`) to
    /// enabled (`true`), or `false` if unknown or already enabled.
    pub fn reenable(&self, mod_id: ModId, system_name: &str) -> bool {
        let Some(flag) = self.systems.get(&(mod_id, Arc::from(system_name))) else {
            return false;
        };
        !flag.swap(true, Ordering::AcqRel)
    }

    /// Iterates over all `(ModId, &str)` pairs that are currently quarantined.
    pub fn quarantined_systems(&self) -> impl Iterator<Item = (ModId, &str)> + '_ {
        self.systems.iter().filter_map(|((id, name), flag)| {
            (!flag.load(Ordering::Acquire)).then_some((*id, name.as_ref()))
        })
    }
}

/// Diagnostic reason why a guest system failed query validation or `System::initialize`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SystemInitError {
    /// A single query group declared more than [`bevy_abi_mod_sys::MAX_TERMS_PER_QUERY`] terms.
    #[error("query group exceeds MAX_TERMS_PER_QUERY (32)")]
    TooManyTerms,

    /// Query term at `index` carried an invalid `AccessMode` discriminant.
    #[error("invalid AccessMode at term index {index}")]
    InvalidMode {
        /// Zero-based term index within the query group.
        index: usize,
    },

    /// Referenced component or resource handle is not registered in [`crate::ModRegistry`].
    #[error("unregistered component or resource '{0}'")]
    UnknownComponent(String),

    /// Resource access mode used on an entity component or entity mode used on a resource.
    #[error("component/resource kind mismatch on '{0}' between access mode and registry")]
    KindMismatch(String),

    /// Mutable access requested on an immutable component (`BAM_COMPONENT_FLAG_IMMUTABLE`).
    #[error("mutable access denied on immutable component '{0}'")]
    ImmutableWrite(String),

    /// Mutually conflicting accesses within one query group or across multiple query groups.
    #[error("conflicting accesses between '{a}' and '{b}'")]
    SelfConflict {
        /// First conflicting component or resource name.
        a: String,
        /// Second conflicting component or resource name.
        b: String,
    },
}

impl SystemInitError {
    pub(crate) const fn status_code(&self) -> FfiResult {
        match self {
            Self::SelfConflict { .. } => FfiResult::ACCESS_CONFLICT,
            Self::TooManyTerms => FfiResult::CAPACITY_EXCEEDED,
            Self::InvalidMode { .. } => FfiResult::INVALID_ARGUMENT,
            Self::ImmutableWrite(_) => FfiResult::PERMISSION_DENIED,
            Self::UnknownComponent(_) | Self::KindMismatch(_) => FfiResult::INVALID_COMPONENT,
        }
    }
}

/// Typed error reported to Bevy's [`FallbackErrorHandler`](bevy_ecs::error::FallbackErrorHandler)
/// when [`ModErrorPolicy::ReportToBevy`] or [`ModErrorPolicy::DelegateToBevyFallback`] is active
/// and a guest system faults.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub struct ModSystemError {
    /// Optional [`ModId`] owning the faulting system.
    pub mod_id: Option<ModId>,
    /// Optional canonical module namespace (`BamManifest::name`).
    pub mod_namespace: Option<String>,
    /// Reload generation of the owning module when the system was initialized.
    pub generation: u64,
    /// Registered name of the faulting guest system.
    pub system_name: String,
    /// Error status returned by the callback (or `FfiResult::INTERNAL_ERROR` for fault reports).
    pub status: FfiResult,
    /// Whether the guest reported a caught panic (`FfiResult::PANIC_CAUGHT`).
    pub panicked: bool,
    /// Optional structured guest diagnostic / fault message captured from `last_error` or
    /// `host_report_guest_fault`.
    pub last_error: Option<String>,
}

impl fmt::Display for ModSystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "guest mod system '{}' (mod_id={:?}, namespace={}, gen={}) faulted with {} (panicked={})",
            self.system_name,
            self.mod_id,
            self.mod_namespace.as_deref().unwrap_or("unscoped"),
            self.generation,
            self.status,
            self.panicked,
        )?;
        if let Some(detail) = self.last_error.as_deref()
            && !detail.is_empty()
        {
            write!(f, ": {detail}")?;
        }
        Ok(())
    }
}

impl ModSystemError {
    /// Converts this [`ModSystemError`] into a contextualized [`BevyError`] (`.with_context(...)`)
    /// with [`Severity::Error`] so Bevy 0.20's default `FallbackErrorHandler` (`match_severity`)
    /// reports the fault cleanly without aborting the process.
    pub fn into_bevy_error(self) -> BevyError {
        self.into_bevy_error_with_severity(Severity::Error)
    }

    /// Converts this [`ModSystemError`] into a contextualized [`BevyError`] (`.with_context(...)`)
    /// tagged with `severity`, preserving `downcast_ref::<ModSystemError>()`.
    pub fn into_bevy_error_with_severity(self, severity: Severity) -> BevyError {
        let mod_id = self.mod_id;
        let sys_name = self.system_name.clone();
        let status = self.status;
        let ns = self
            .mod_namespace
            .clone()
            .unwrap_or_else(|| String::from("unscoped"));
        let generation = self.generation;
        let detail = self.last_error.clone();
        let err_res: Result<(), Self> = Err(self);
        err_res
            .with_context(|| match detail.as_deref() {
                Some(msg) if !msg.is_empty() => format!(
                    "guest mod {mod_id:?} system {sys_name:?} faulted with {status} [namespace={ns}, generation={generation}, last_error={msg:?}]"
                ),
                _ => format!(
                    "guest mod {mod_id:?} system {sys_name:?} faulted with {status} [namespace={ns}, generation={generation}]"
                ),
            })
            .unwrap_err()
            .with_severity(severity)
    }
}

/// Dispatches a structured [`ModSystemError`] directly to `world`'s Bevy `0.20`
/// [`FallbackErrorHandler`](bevy_ecs::error::FallbackErrorHandler) (`world.fallback_error_handler()`)
/// with [`ErrorContext::System`] and [`Severity::Error`].
pub fn dispatch_mod_fault_to_fallback_handler(
    world: &World,
    error: ModSystemError,
    last_run: Tick,
) {
    let sys_name = error.system_name.clone();
    let bevy_err = error.into_bevy_error();
    let handler = world.fallback_error_handler();
    handler(
        bevy_err,
        ErrorContext::System {
            name: DebugName::owned(sys_name),
            last_run,
        },
    );
}

/// Buffered Bevy [`Message`] emitted once via `Messages<ModSystemFaulted>` whenever a guest system
/// faults at initialization or runtime.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ModSystemFaulted {
    /// [`ModId`] owning the faulting system, if tracked.
    pub mod_id: Option<ModId>,
    /// Canonical module namespace (`BamManifest::name`), if tracked.
    pub mod_namespace: Option<String>,
    /// Reload generation of the owning module when the system was initialized.
    pub generation: u64,
    /// Name of the faulting guest system.
    pub system_name: String,
    /// Fault [`FfiResult`] status code.
    pub status: FfiResult,
    /// `true` if the guest reported a caught panic (`FfiResult::PANIC_CAUGHT`).
    pub panicked: bool,
    /// Optional structured guest diagnostic / fault message captured from `last_error` or
    /// `host_report_guest_fault`.
    pub last_error: Option<String>,
    /// Structured initialization failure when the fault occurred during `System::initialize`.
    pub init_error: Option<SystemInitError>,
}

/// Metadata returned to `host_report_guest_fault` when a fault is reported inside an active
/// [`crate::GuestSystem`] callback on the current thread.
#[derive(Debug, Clone)]
pub(crate) struct ActiveGuestFaultMeta {
    pub(crate) mod_id: Option<ModId>,
    pub(crate) mod_namespace: Option<Arc<str>>,
    pub(crate) generation: u64,
    pub(crate) system_name: Arc<str>,
}

#[derive(Debug)]
struct ActiveGuestFaultSlot {
    host_ctx_addr: usize,
    meta: ActiveGuestFaultMeta,
    reported_message: Option<String>,
}

thread_local! {
    static ACTIVE_GUEST_FAULT: RefCell<Option<ActiveGuestFaultSlot>> = const { RefCell::new(None) };
}

/// Scoped RAII guard binding the current thread's active `GuestSystem` context so
/// `host_report_guest_fault` can attach structured fault context without modifying `HostContext`.
pub(crate) struct ActiveGuestFaultGuard {
    previous: Option<ActiveGuestFaultSlot>,
    finished: bool,
}

impl ActiveGuestFaultGuard {
    /// Installs an active guest-fault capture slot for `host_ctx_ptr` on the current thread.
    pub(crate) fn enter(
        host_ctx_ptr: *const BamHostCtx,
        mod_id: Option<ModId>,
        mod_namespace: Option<Arc<str>>,
        generation: u64,
        system_name: Arc<str>,
    ) -> Self {
        let slot = ActiveGuestFaultSlot {
            host_ctx_addr: host_ctx_ptr as usize,
            meta: ActiveGuestFaultMeta {
                mod_id,
                mod_namespace,
                generation,
                system_name,
            },
            reported_message: None,
        };
        let previous = ACTIVE_GUEST_FAULT.with(|cell| cell.borrow_mut().replace(slot));
        Self {
            previous,
            finished: false,
        }
    }

    /// Restores the previous thread-local slot and returns any message recorded via
    /// `host_report_guest_fault` during this scope.
    pub(crate) fn finish(mut self) -> Option<String> {
        self.finished = true;
        let prev = self.previous.take();
        ACTIVE_GUEST_FAULT
            .with(|cell| core::mem::replace(&mut *cell.borrow_mut(), prev))
            .and_then(|slot| slot.reported_message)
    }
}

impl Drop for ActiveGuestFaultGuard {
    fn drop(&mut self) {
        if !self.finished {
            let prev = self.previous.take();
            let _ = ACTIVE_GUEST_FAULT.try_with(|cell| {
                *cell.borrow_mut() = prev;
            });
        }
    }
}

/// Records `message` into the current thread's active [`crate::GuestSystem`] fault capture slot
/// when `host_ctx_ptr` matches the active system's [`crate::HostContext`], returning the active
/// system's structured metadata for diagnostic logging.
pub(crate) fn record_active_guest_fault(
    host_ctx_ptr: *const BamHostCtx,
    message: &str,
) -> Option<ActiveGuestFaultMeta> {
    let addr = host_ctx_ptr as usize;
    ACTIVE_GUEST_FAULT
        .try_with(|cell| {
            let mut guard = cell.borrow_mut();
            let slot = guard.as_mut()?;
            if slot.host_ctx_addr != addr {
                return None;
            }
            slot.reported_message = Some(message.to_owned());
            Some(slot.meta.clone())
        })
        .ok()
        .flatten()
}

/// Execution and quota metrics captured for one [`crate::GuestSystem`] invocation and passed to
/// [`ModBudgetHook::record_system_run`].
#[derive(Clone, Debug)]
pub struct GuestSystemRunMetrics {
    /// Owning [`ModId`], if associated with a mod.
    pub mod_id: Option<ModId>,
    /// Canonical module namespace (`BamManifest::name` or `"unscoped"`).
    pub mod_name: Arc<str>,
    /// Registered name of the guest system.
    pub system_name: Arc<str>,
    /// Wall-clock callback execution duration in microseconds.
    pub duration_us: u64,
    /// Number of entities matched or processed during this invocation.
    pub matched_entities: u64,
    /// Number of non-empty ECS tables/archetypes matched by this system's queries.
    pub matched_tables: u64,
    /// Number of matched tables skipped via `BAM_COMPONENT_FLAG_SUMMARY_TICK`.
    pub skipped_summary_tick_tables: u64,
    /// Number of query chunks yielded during this invocation.
    pub chunks_yielded: usize,
    /// Number of deferred commands accepted during this invocation (`0` when faulted).
    pub commands_queued: usize,
    /// Peak `CommandArena` payload bytes bump-allocated during this invocation.
    pub arena_bytes_used: usize,
    /// Number of deferred entity spawns accepted during this invocation (`0` when faulted).
    pub spawns_queued: usize,
    /// Number of command payload allocations rejected due to `max_arena_bytes`.
    pub arena_breaches: usize,
    /// Number of deferred entity spawns rejected due to `max_spawns_per_frame`.
    pub spawn_breaches: usize,
    /// `true` if the guest callback returned an error, panicked, or reported a fault.
    pub faulted: bool,
}

/// Immediate watchdog action returned by [`ModBudgetHook::record_system_run`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BudgetWatchdogAction {
    /// When `true`, [`crate::GuestSystem`] immediately quarantines itself (`enabled = false`),
    /// increments `ModHost` fault count, and emits [`ModSystemFaulted`].
    pub quarantine: bool,
    /// Optional diagnostic summary recorded in `ModSystemFaulted::last_error`.
    pub reason: Option<String>,
}

/// Safe runtime quota & frame-time watchdog hook installed into [`ModBudgetRuntime`] by
/// `bevy_abi_mod_host` (`#![forbid(unsafe_code)]`).
pub trait ModBudgetHook: Send + Sync + 'static {
    /// Resolves active [`CommandQuotaLimits`] and an optional shared per-mod frame spawn counter
    /// enforced atomically across parallel systems in the same frame.
    fn prepare_system_quotas(
        &self,
        mod_id: Option<ModId>,
        mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>);

    /// Records completed execution metrics for a [`crate::GuestSystem`] run and returns any
    /// immediate watchdog quarantine decision.
    fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction;
}

/// World [`Resource`] bridging [`crate::GuestSystem`] and [`crate::HostCommands`] to a safe
/// [`ModBudgetHook`] implementation without placing policy logic inside `bevy_abi_mod_host_sys`.
#[derive(Resource, Clone)]
pub struct ModBudgetRuntime {
    hook: Arc<dyn ModBudgetHook>,
}

impl ModBudgetRuntime {
    /// Creates a new [`ModBudgetRuntime`] backed by `hook`.
    #[inline]
    pub fn new(hook: Arc<dyn ModBudgetHook>) -> Self {
        Self { hook }
    }

    /// Resolves active [`CommandQuotaLimits`] and optional shared per-mod frame spawn counter for
    /// `(mod_id, mod_name)`.
    #[inline]
    pub fn prepare_system_quotas(
        &self,
        mod_id: Option<ModId>,
        mod_name: &str,
    ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
        self.hook.prepare_system_quotas(mod_id, mod_name)
    }

    /// Records completed execution metrics for a guest system run and returns any watchdog action.
    #[inline]
    pub fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
        self.hook.record_system_run(metrics)
    }
}

impl fmt::Debug for ModBudgetRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModBudgetRuntime").finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct DummyBudgetHook {
        limits: CommandQuotaLimits,
        counter: Arc<AtomicUsize>,
        recorded: Mutex<Vec<GuestSystemRunMetrics>>,
        trigger_quarantine: bool,
    }

    impl ModBudgetHook for DummyBudgetHook {
        fn prepare_system_quotas(
            &self,
            _mod_id: Option<ModId>,
            _mod_name: &str,
        ) -> (CommandQuotaLimits, Option<Arc<AtomicUsize>>) {
            (self.limits, Some(Arc::clone(&self.counter)))
        }

        fn record_system_run(&self, metrics: GuestSystemRunMetrics) -> BudgetWatchdogAction {
            self.recorded.lock().unwrap().push(metrics);
            BudgetWatchdogAction {
                quarantine: self.trigger_quarantine,
                reason: self
                    .trigger_quarantine
                    .then(|| String::from("p99 budget exceeded")),
            }
        }
    }

    #[test]
    fn mod_budget_runtime_delegates_prepare_and_record() {
        let hook = Arc::new(DummyBudgetHook {
            limits: CommandQuotaLimits {
                max_arena_bytes: Some(256),
                max_spawns_per_frame: Some(4),
            },
            counter: Arc::new(AtomicUsize::new(0)),
            recorded: Mutex::new(Vec::new()),
            trigger_quarantine: true,
        });
        let runtime = ModBudgetRuntime::new(Arc::clone(&hook) as Arc<dyn ModBudgetHook>);
        assert!(format!("{runtime:?}").contains("ModBudgetRuntime"));

        let (limits, shared) = runtime.prepare_system_quotas(Some(ModId(9)), "test_mod");
        assert_eq!(limits.max_arena_bytes, Some(256));
        assert_eq!(limits.max_spawns_per_frame, Some(4));
        assert!(shared.is_some());

        let action = runtime.record_system_run(GuestSystemRunMetrics {
            mod_id: Some(ModId(9)),
            mod_name: Arc::from("test_mod"),
            system_name: Arc::from("tick"),
            duration_us: 999,
            matched_entities: 10,
            matched_tables: 2,
            skipped_summary_tick_tables: 1,
            chunks_yielded: 1,
            commands_queued: 3,
            arena_bytes_used: 128,
            spawns_queued: 2,
            arena_breaches: 0,
            spawn_breaches: 1,
            faulted: false,
        });
        assert!(action.quarantine);
        assert_eq!(action.reason.as_deref(), Some("p99 budget exceeded"));
        assert_eq!(hook.recorded.lock().unwrap().len(), 1);
    }

    #[test]
    fn error_policy_display_quarantine_fault_guard_drop_and_init_codes() {
        let world = World::new();

        // `SystemInitError::status_code` coverage
        assert_eq!(
            SystemInitError::TooManyTerms.status_code(),
            FfiResult::CAPACITY_EXCEEDED
        );
        assert_eq!(
            SystemInitError::InvalidMode { index: 0 }.status_code(),
            FfiResult::INVALID_ARGUMENT
        );
        assert_eq!(
            SystemInitError::UnknownComponent(String::from("x")).status_code(),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            SystemInitError::KindMismatch(String::from("x")).status_code(),
            FfiResult::INVALID_COMPONENT
        );
        assert_eq!(
            SystemInitError::ImmutableWrite(String::from("x")).status_code(),
            FfiResult::PERMISSION_DENIED
        );
        assert_eq!(
            SystemInitError::SelfConflict {
                a: String::from("a"),
                b: String::from("b")
            }
            .status_code(),
            FfiResult::ACCESS_CONFLICT
        );

        // `ModSystemError` Display (with and without `last_error`) and `dispatch_mod_fault_to_fallback_handler`
        let err_plain = ModSystemError {
            mod_id: None,
            mod_namespace: None,
            generation: 1,
            system_name: String::from("plain_sys"),
            status: FfiResult::INVALID_STATE,
            panicked: false,
            last_error: None,
        };
        assert!(format!("{err_plain}").contains("unscoped"));
        let _ = err_plain.into_bevy_error();

        let err_detail = ModSystemError {
            mod_id: Some(ModId(3)),
            mod_namespace: Some(String::from("ns")),
            generation: 2,
            system_name: String::from("detail_sys"),
            status: FfiResult::PANIC_CAUGHT,
            panicked: true,
            last_error: Some(String::from("panic detail")),
        };
        assert!(format!("{err_detail}").contains("panic detail"));
        dispatch_mod_fault_to_fallback_handler(&world, err_detail, Tick::new(1));

        // `ModQuarantine` lifecycle
        let mut q = ModQuarantine::new();
        let flag = Arc::new(AtomicBool::new(false));
        q.register_system(ModId(1), Arc::from("q_sys"), Arc::clone(&flag));
        assert!(q.is_quarantined(ModId(1), "q_sys"));
        let list: Vec<_> = q.quarantined_systems().collect();
        assert_eq!(list, vec![(ModId(1), "q_sys")]);
        assert!(q.reenable(ModId(1), "q_sys"));
        assert!(!q.reenable(ModId(1), "q_sys"));
        assert!(!q.reenable(ModId(99), "missing"));
        q.unregister_mod(ModId(1));
        assert!(!q.is_quarantined(ModId(1), "q_sys"));

        // `ActiveGuestFaultGuard::drop` restores previous slot when dropped without `finish()`,
        // and `record_active_guest_fault` returns `None` for non-matching `host_ctx_ptr`.
        let mut ctx_a = 1u64;
        let mut ctx_b = 2u64;
        let ptr_a = (&raw mut ctx_a).cast::<BamHostCtx>();
        let ptr_b = (&raw mut ctx_b).cast::<BamHostCtx>();
        let outer =
            ActiveGuestFaultGuard::enter(ptr_a, Some(ModId(1)), None, 0, Arc::from("outer"));
        {
            let _inner =
                ActiveGuestFaultGuard::enter(ptr_b, Some(ModId(2)), None, 0, Arc::from("inner"));
            assert!(record_active_guest_fault(ptr_a, "wrong ctx").is_none());
            assert!(record_active_guest_fault(ptr_b, "inner msg").is_some());
        }
        assert!(record_active_guest_fault(ptr_a, "outer msg").is_some());
        assert_eq!(outer.finish().as_deref(), Some("outer msg"));
    }
}
