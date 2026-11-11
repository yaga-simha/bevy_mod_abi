//! Low-level raw FFI types, `unsafe` extension traits, and `bevy_abi_mod_guest_sys` re-exports.
//!
//! Normal guest mods should use [`GuestApp`](crate::GuestApp), [`SystemContext`](crate::SystemContext),
//! and [`MultiQueryContext`](crate::MultiQueryContext) instead of constructing handles manually.
//! Callers working directly with raw C-ABI callbacks (`DynamicSystemCallback`,
//! `MultiQuerySystemCallback`, or manual `bevy_abi_mod_init` implementations) must uphold the
//! `# Safety` contracts documented on [`bevy_abi_mod_guest_sys`].

pub use bevy_abi_mod_guest_sys::{
    ChunkSliceMutRawExt, ChunkSliceRawExt, CommandBridgeVTable, CommandBridgeVTableExt,
    GuestAppInitExt, GuestAppLifecycle, GuestAppRawExt, GuestChunkIterRawExt,
    GuestCommandQueueRawExt, MAX_GUEST_MULTI_QUERIES, MIN_GUEST_APP_HOST_API_SIZE,
    MIN_REGISTER_COMPONENT_HOST_API_SIZE, ResourceBridgeVTable, ValidatedCommandQueue,
    ValidatedInit, ValidatedIter, ValidatedSlice, ValidatedSliceMut, catch_guest_panic,
    cleanup_registered_guest_state, register_runtime_component, resolve_fn, resolve_iter_reset,
    resolve_register_conditional_multi_query, resolve_register_conditional_system,
    resolve_register_multi_query, resolve_symbol_cstr, resolve_vtable_raw, run_lifecycle_init,
    run_with_validated_init, validate_host_api, wrap_multi_query_callback, wrap_run_condition,
    wrap_system_callback,
};
