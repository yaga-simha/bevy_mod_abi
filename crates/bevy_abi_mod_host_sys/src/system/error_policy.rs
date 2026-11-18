//! Fault policies, `ModQuarantine` resource, runtime system errors, and initialization diagnostics
//!.

use crate::loader::ModId;
use bevy_abi_mod_sys::FfiResult;
use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use bevy_platform::collections::HashMap;
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
    /// with a contextualized [`BevyError`](bevy_ecs::error::BevyError) (`.with_context(...)`)
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

/// Typed error reported to Bevy when [`ModErrorPolicy::ReportToBevy`] is active and a guest system
/// faults.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error(
    "guest mod system '{system_name}' (mod_id={mod_id:?}) faulted with {status} (panicked={panicked})"
)]
pub struct ModSystemError {
    /// Optional [`ModId`] owning the faulting system.
    pub mod_id: Option<ModId>,
    /// Registered name of the faulting guest system.
    pub system_name: String,
    /// Error status returned by the callback.
    pub status: FfiResult,
    /// Whether the guest reported a caught panic (`FfiResult::PANIC_CAUGHT`).
    pub panicked: bool,
}

/// Buffered Bevy [`Message`] emitted once via `Messages<ModSystemFaulted>` whenever a guest system
/// faults at initialization or runtime.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct ModSystemFaulted {
    /// [`ModId`] owning the faulting system, if tracked.
    pub mod_id: Option<ModId>,
    /// Name of the faulting guest system.
    pub system_name: String,
    /// Fault [`FfiResult`] status code.
    pub status: FfiResult,
    /// `true` if the guest reported a caught panic (`FfiResult::PANIC_CAUGHT`).
    pub panicked: bool,
    /// Structured initialization failure when the fault occurred during `System::initialize`.
    pub init_error: Option<SystemInitError>,
}
