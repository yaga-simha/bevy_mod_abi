//! Idiomatic prelude for Rust guest mods (`use bevy_abi_mod_guest::prelude::*;`).

pub use bevy_abi_mod_guest_sys::SpawnEntry;
pub use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_SYSTEM_FLAG_MAIN_THREAD, BamColumnInfo, BamComponent, BamManifest, ComponentUuid, FfiBool,
    FfiEntity, FfiResult, FfiSlice, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent,
    RawInsertable, ScheduleId, StrView, VTableHeader,
};

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::{AbiPod, ModComponent};

pub use crate::{
    app::{
        GuestApp, MultiQueryContext, QueryBuilder, RunConditionContext, SystemBuilder,
        SystemContext,
    },
    commands::GuestCommands,
    component::{GuestComponent, GuestComponentHooks, guest_component_uuid},
    export_mod,
    iter::{
        Added, Changed, Chunk, ChunkIter, ChunkSlice, ChunkSliceMut, QueryTerm, QueryTuple,
        SparseRows, SparseRowsMut, TypedChunkIter, With, Without,
    },
    lifecycle::GuestPlugin,
};
