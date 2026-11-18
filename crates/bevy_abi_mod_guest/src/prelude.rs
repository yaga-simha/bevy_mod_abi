//! Idiomatic prelude for Rust guest mods (`use bevy_abi_mod_guest::prelude::*;`).

pub use bevy_abi_mod_guest_sys::SpawnEntry;
pub use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_OBSERVER_FLAG_PROPAGATE,
    BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_SYSTEM_FLAG_MAIN_THREAD, BAM_TRIGGER_FLAG_PROPAGATE,
    BamChangeTicks, BamColumnInfo, BamComponent, BamEventKind, BamManifest, BamObserverDesc,
    BamObserverFn, BamObserverMutFn, BamTriggerDesc, ComponentUuid, FfiAssetHandle,
    FfiAssetLoadState, FfiBool, FfiChildOf, FfiEntity, FfiOption, FfiResult, FfiSlice,
    MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent, RawInsertable, ScheduleId, StrView,
    VTableHeader,
};

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::{AbiPod, ModComponent};

#[cfg(feature = "derive")]
pub use crate::bridges::{
    GuestGlobalTransformBridge, GuestKeyboardInputBridge, GuestMouseInputBridge,
    GuestTimeFixedBridge, GuestTimeRealBridge, GuestTimeVirtualBridge, GuestTransformBridge,
    GuestVisibilityBridge, GuestWireBridge,
};
pub use crate::{
    app::{
        GuestApp, GuestComponentOptions, GuestRes, GuestResMut, MultiQueryContext, QueryBuilder,
        RunConditionContext, SystemBuilder, SystemContext,
    },
    bridges::{
        FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput,
        FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, GuestAssetHandle,
        GuestAssetHostExt, GuestAssetServer, GuestCommandsStateExt, GuestNameBridge,
        GuestStateBridge, GuestStateSystemBuilderExt, TypedAssetHandle, on_enter_schedule,
        on_exit_schedule, on_transition_schedule,
    },
    commands::GuestCommands,
    component::{GuestComponent, GuestComponentHooks, guest_component_uuid},
    export_mod,
    iter::{
        Added, Allow, Changed, Chunk, ChunkIter, ChunkSlice, ChunkSliceMut, Has, Or2, Or3,
        QueryTerm, QueryTuple, SparseRows, SparseRowsMut, TypedChunkIter, With, Without,
    },
    lifecycle::GuestPlugin,
};
