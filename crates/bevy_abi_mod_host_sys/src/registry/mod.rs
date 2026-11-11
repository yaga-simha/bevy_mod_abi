//! Component, resource, VTable, and symbol registration catalog.

pub mod catalog;
pub mod collision;
pub(crate) mod symbols;

pub use catalog::{InsertDefaultFn, InsertRawFn, ModRegistry, RegisteredComponent};
pub use collision::ComponentCollisionError;
#[cfg(any(test, feature = "test-utils"))]
pub use symbols::CurrentRegistryGuard;
