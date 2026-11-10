#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # `host_runtime`
//!
//! Bevy ECS host runtime bridge, component registry, chunk iterator,
//! dynamic system adapter, and `libloading` lifecycle manager for `bevy_abi_mod`.

pub mod chunk_iter;
pub mod commands;
pub mod dynamic_system;
pub mod host;
pub mod plugin;
pub mod registry;

use abi_contract::{HostApi, ABI_VERSION_PACKED};

// Real static HOST_API exposing concrete engine bridge functions across the C-ABI.
pub static HOST_API: HostApi = HostApi {
    struct_size: core::mem::size_of::<HostApi>() as u32,
    abi_version: ABI_VERSION_PACKED,
    register_system: dynamic_system::host_register_system,
    iter_next_chunk: chunk_iter::host_iter_next_chunk,
    iter_column_readonly: chunk_iter::host_iter_column_readonly,
    iter_column_mutable: chunk_iter::host_iter_column_mutable,
    iter_mark_changed: chunk_iter::host_iter_mark_changed,
    iter_entities: chunk_iter::host_iter_entities,
    iter_column_stride: chunk_iter::host_iter_column_stride,
};

// Re-exports
pub use abi_contract;
pub use mod_derive::ModExposable;

pub use chunk_iter::{
    host_iter_column_mutable, host_iter_column_readonly, host_iter_column_stride,
    host_iter_entities, host_iter_mark_changed, host_iter_next_chunk, HostChunkIter,
    QueryColumnDesc, MAX_QUERY_COMPONENTS,
};
pub use commands::{
    host_cmd_despawn, host_cmd_insert_component, host_cmd_spawn_empty, CmdDespawnFn,
    CmdInsertComponentFn, CmdSpawnEmptyFn, CommandQueueBridge,
};
pub use dynamic_system::{
    host_register_system, ComponentAccessDescOwned, DynamicCSystem, ModInitContext,
};
pub use host::{LoadedMod, ModHost, ModId};
pub use plugin::{AbiModPlugin, ModAppExt};
pub use registry::{
    host_get_proc_address, CurrentRegistryGuard, InsertRawFn, ModRegistry, RegisteredComponent,
    CURRENT_REGISTRY, GLOBAL_SYMBOLS,
};

/// Convenient prelude module for Bevy games hosting native C-ABI mods.
pub mod prelude {
    pub use crate::host::{LoadedMod, ModHost, ModId};
    pub use crate::plugin::{AbiModPlugin, ModAppExt};
    pub use crate::registry::ModRegistry;
    pub use crate::ModExposable;
    pub use abi_contract::{
        ComponentAccessDesc, ComponentAccessMode, ComponentUuid, ExposableComponent, FfiEntity,
        FfiResult, FfiSlice, HostApi, StrView, SystemDescriptor, ABI_VERSION_MAJOR,
        ABI_VERSION_MINOR, ABI_VERSION_PACKED, ABI_VERSION_PATCH,
    };
}
