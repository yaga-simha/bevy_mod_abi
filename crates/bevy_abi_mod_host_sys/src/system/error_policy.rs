//! Fault policies, runtime system errors, and initialization diagnostics (`SystemInitError`).

use crate::loader::ModId;
use bevy_abi_mod_sys::FfiResult;
use bevy_ecs::event::Event;
use bevy_ecs::message::Message;
use bevy_ecs::prelude::Resource;
use core::fmt;

/// Error-handling policy governing how [`crate::DynamicCSystem`] responds when a guest callback
/// returns an error [`FfiResult`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ModErrorPolicy {
    /// Quarantines the faulting guest system (`enabled = false`) and returns `Ok(())` (default).
    #[default]
    Quarantine,
    /// Quarantines the faulting guest system, logs diagnostic details to stderr, and returns `Ok(())`.
    Log,
    /// Quarantines the faulting guest system and propagates [`ModSystemError`] via
    /// `Err(RunSystemError::Failed(...))` to Bevy's error handler.
    ReportToBevy,
}

/// World resource configuring the default [`ModErrorPolicy`] for dynamic guest systems.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModErrorPolicyConfig(pub ModErrorPolicy);

/// Diagnostic reason why a guest system failed query validation or `System::initialize`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SystemInitError {
    /// A single query group declared more than [`crate::ffi::iter::MAX_QUERY_COMPONENTS`] terms.
    TooManyTerms,
    /// Query term at `index` carried an invalid `ComponentAccessMode` discriminant.
    InvalidMode {
        /// Zero-based term index within the query group.
        index: usize,
    },
    /// Referenced component or resource name is not registered in [`crate::ModRegistry`].
    UnknownComponent(String),
    /// Resource access mode used on an entity component or entity mode used on a resource.
    KindMismatch(String),
    /// Mutually conflicting accesses within one query group or across multiple query groups.
    SelfConflict {
        /// First conflicting component or resource name.
        a: String,
        /// Second conflicting component or resource name.
        b: String,
    },
}

impl SystemInitError {
    /// Maps this initialization failure to its corresponding C-ABI [`FfiResult`] code.
    #[inline]
    pub const fn to_ffi_result(&self) -> FfiResult {
        match self {
            Self::SelfConflict { .. } => FfiResult::ACCESS_CONFLICT,
            Self::TooManyTerms => FfiResult::OUT_OF_RANGE,
            Self::InvalidMode { .. } => FfiResult::INVALID_ARGUMENT,
            Self::UnknownComponent(_) | Self::KindMismatch(_) => FfiResult::INVALID_COMPONENT,
        }
    }
}

impl fmt::Display for SystemInitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooManyTerms => write!(f, "query group exceeds MAX_QUERY_COMPONENTS (32)"),
            Self::InvalidMode { index } => {
                write!(f, "invalid ComponentAccessMode at term index {index}")
            }
            Self::UnknownComponent(name) => {
                write!(f, "unregistered component or resource '{name}'")
            }
            Self::KindMismatch(name) => {
                write!(
                    f,
                    "component/resource kind mismatch on '{name}' between access mode and registry"
                )
            }
            Self::SelfConflict { a, b } => {
                write!(f, "conflicting accesses between '{a}' and '{b}'")
            }
        }
    }
}

impl std::error::Error for SystemInitError {}

/// Typed error reported to Bevy when [`ModErrorPolicy::ReportToBevy`] is active and a guest system faults.
#[derive(Debug, Clone, PartialEq, Eq)]
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

impl fmt::Display for ModSystemError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "guest mod system '{}' (mod_id={:?}) faulted with {:?} (panicked={})",
            self.system_name, self.mod_id, self.status, self.panicked
        )
    }
}

impl std::error::Error for ModSystemError {}

/// Bevy [`Event`] and buffered [`Message`] emitted whenever a guest system faults at initialization
/// or runtime and is quarantined.
#[derive(Event, Message, Debug, Clone, PartialEq, Eq)]
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
