#![deny(
    improper_ctypes_definitions,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]
//! Low-level `unsafe` FFI and Bevy ECS storage internals for `bevy_abi_mod_host`.
//!
//! Isolates all host-side `unsafe` code (chunk iteration over `UnsafeWorldCell`, deferred raw
//! byte commands, `DynamicCSystem`, `ModRegistry` VTable/symbol lookup, drop trampolines, and
//! `libloading` shared-library lifecycle) behind audited invariants so `bevy_abi_mod_host` and
//! `bevy_abi_mod` enforce `#![forbid(unsafe_code)]`.

pub mod drop_trampoline;
#[cfg(any(test, feature = "test-utils"))]
pub mod ffi;
#[cfg(not(any(test, feature = "test-utils")))]
#[allow(unreachable_pub)]
pub(crate) mod ffi;
pub mod loader;
pub mod registry;
pub mod schedule;
pub mod sync_ptr;
pub mod system;

pub use drop_trampoline::MAX_DROP_TRAMPOLINES;
#[cfg(any(test, feature = "test-utils"))]
pub use drop_trampoline::reset_drop_trampolines_for_testing;
pub use ffi::host_api::HOST_API;
pub use loader::{
    LoadedMod, ModHost, ModId, ModLoadError, canonical_mod_name_from_path,
    excise_and_unload_mod_in_world, is_abi_version_compatible, load_mod_in_world,
    record_mod_named_sets_in_world, rollback_unscheduled_mod_in_world, unload_all_mods_in_world,
};
pub use registry::{
    ComponentCollisionError, InsertDefaultFn, InsertRawFn, ModRegistry, RegisteredComponent,
};
pub use schedule::{
    ModNamedSystemSet, ModSchedule, ModSystemSet, StartupCompleted, mark_startup_completed,
};
pub use sync_ptr::{GuestFnPtr, GuestUserData, SymbolAddress};
pub use system::{
    ComponentAccessDescOwned, DynamicCSystem, MAX_SYSTEM_QUERIES, ModErrorPolicy,
    ModErrorPolicyConfig, ModSystemError, ModSystemFaulted, SystemInitError,
};

#[cfg(any(test, feature = "test-utils"))]
pub use ffi::commands::{
    CommandQueueBridge, host_cmd_despawn, host_cmd_insert_component, host_cmd_insert_default,
    host_cmd_remove_component, host_cmd_spawn_empty,
};
#[cfg(any(test, feature = "test-utils"))]
pub use ffi::iter::{
    HostChunkIter, MAX_QUERY_COMPONENTS, QueryColumnDesc, host_iter_column_mutable,
    host_iter_column_readonly, host_iter_column_stride, host_iter_entities, host_iter_mark_changed,
    host_iter_next_chunk, host_iter_reset, host_iter_resource_mutable, host_iter_resource_readonly,
};
#[cfg(any(test, feature = "test-utils"))]
pub use ffi::proc_address::host_get_proc_address;
#[cfg(any(test, feature = "test-utils"))]
pub use ffi::register::{
    ActiveInitContextGuard, MOD_INIT_CONTEXT_MAGIC, ModInitContext, ModInitContextHeader,
    host_register_component, host_register_conditional_multi_query_system,
    host_register_conditional_system, host_register_multi_query_system,
    host_register_scheduled_system, host_register_system, host_register_system_in_schedule,
};
#[cfg(any(test, feature = "test-utils"))]
pub use registry::CurrentRegistryGuard;
