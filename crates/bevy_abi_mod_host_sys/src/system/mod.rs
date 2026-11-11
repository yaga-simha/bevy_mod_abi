//! Dynamic guest ECS system adapter, query access validation, and fault policies.

pub(crate) mod access_builder;
pub mod error_policy;
pub mod guest_system;
pub mod query_group;

pub use error_policy::{
    ModErrorPolicy, ModErrorPolicyConfig, ModSystemError, ModSystemFaulted, SystemInitError,
};
pub use guest_system::DynamicCSystem;
pub use query_group::{ComponentAccessDescOwned, MAX_SYSTEM_QUERIES};
