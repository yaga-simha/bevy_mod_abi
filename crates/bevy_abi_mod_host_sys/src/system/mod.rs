//! Dynamic guest ECS system adapter (`GuestSystem`), query access validation, and fault policies.

pub(crate) mod access_builder;
pub mod error_policy;
pub mod guest_system;
pub mod query_group;

pub use error_policy::{
    ModErrorPolicy, ModErrorPolicyConfig, ModQuarantine, ModSystemError, ModSystemFaulted,
    SystemInitError,
};
pub use guest_system::{
    GuestSystem, HostSystemCtx, host_sys_commands, host_sys_entity_get, host_sys_entity_get_mut,
    host_sys_host_ctx, host_sys_query, host_sys_res_mut_ptr, host_sys_res_ptr,
};
#[cfg(feature = "diagnostics")]
pub use guest_system::{ModDiagnosticsCollector, ModDiagnosticsSample};
pub use query_group::{BamQuerySpecOwned, MAX_SYSTEM_QUERIES};
