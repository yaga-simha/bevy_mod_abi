//! Fault policies, `ModQuarantine` resource, runtime system errors, and initialization diagnostics.

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
use core::sync::atomic::{AtomicBool, Ordering};
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
