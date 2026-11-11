//! Convenient prelude re-exports for guest mod authors.

pub use crate::raw::{
    ChunkSliceMutRawExt, ChunkSliceRawExt, CommandBridgeVTable, CommandBridgeVTableExt,
    GuestAppInitExt, GuestAppRawExt, GuestChunkIterRawExt, GuestCommandQueueRawExt,
    MAX_GUEST_MULTI_QUERIES, ResourceBridgeVTable, catch_guest_panic,
    cleanup_registered_guest_state, register_runtime_component, resolve_fn, resolve_iter_reset,
    resolve_register_conditional_multi_query, resolve_register_conditional_system,
    resolve_register_multi_query, wrap_multi_query_callback, wrap_run_condition,
    wrap_system_callback,
};
pub use crate::{
    AbiPod, Chunk, ChunkSlice, ChunkSliceMut, GuestApp, GuestChunkIter, GuestCommandQueue,
    GuestComponent, GuestPlugin, MultiQueryContext, SystemContext, export_mod,
};
pub use bevy_abi_mod_sys::{
    ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH, CmdDespawnFn,
    CmdInsertComponentFn, CmdInsertDefaultFn, CmdRemoveComponentFn, CmdSpawnEmptyFn,
    ComponentAccessDesc, ComponentAccessMode, ComponentDropFn, ComponentUuid,
    ConditionalMultiQuerySystemDescriptor, ConditionalSystemDescriptor, DynamicComponentDescriptor,
    DynamicSystemCallback, ExposableComponent, ExtensibleStruct, FFI_BOOL_FALSE, FFI_BOOL_TRUE,
    FfiBool, FfiEntity, FfiLayout, FfiResult, FfiSlice, GetProcAddressFn, HostApi, IterResetFn,
    IterResourceMutableFn, IterResourceReadonlyFn, ModComponentDescriptor,
    MultiQuerySystemCallback, MultiQuerySystemDescriptor, QuerySpec, RawChunkIter, RawCommandQueue,
    RegisterComponentFn, RegisterConditionalMultiQuerySystemFn, RegisterConditionalSystemFn,
    RegisterMultiQuerySystemFn, RegisterSystemFn, RunConditionCallback,
    ScheduledMultiQuerySystemDescriptor, ScheduledSystemDescriptor, StrView, SymbolEntry,
    SystemDescriptor, ffi_bool_to_rust, rust_bool_to_ffi,
};
