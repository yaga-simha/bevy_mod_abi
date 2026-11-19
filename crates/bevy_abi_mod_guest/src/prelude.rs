//! Idiomatic prelude for Rust guest mods (`use bevy_abi_mod_guest::prelude::*;`).

pub use bevy_abi_mod_guest_sys::SpawnEntry;
pub use bevy_abi_mod_sys::{
    ABI_VERSION, AbiPod, AccessMode, BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET,
    BAM_COMPONENT_FLAG_SUMMARY_TICK, BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE,
    BAM_LOG_WARN, BAM_OBSERVER_FLAG_PROPAGATE, BAM_RELATION_FLAG_ACYCLIC,
    BAM_RELATION_FLAG_CASCADE_DELETE, BAM_RELATION_FLAG_CASCADE_DESPAWN,
    BAM_RELATION_FLAG_CLEANUP_REMOVE, BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC,
    BAM_SCHEDULE_SET_FLAG_AFTER_WEAK, BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK,
    BAM_SCHEDULE_SET_FLAG_PARALLEL, BAM_SCHEDULE_SET_FLAG_SEQUENCE,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK, BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL,
    BAM_SYSTEM_FLAG_MAIN_THREAD, BAM_TRIGGER_FLAG_PROPAGATE, BamChangeTicks, BamColumnInfo,
    BamComponent, BamEventKind, BamFieldDesc, BamFieldKind, BamLayoutInfo, BamManifest,
    BamObserverDesc, BamObserverFn, BamObserverMutFn, BamRelationDesc, BamRequiredComponent,
    BamRowMigrateFn, BamScheduleSetDesc, BamSymbol, BamSystem, BamSystemSet, BamTriggerDesc,
    ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiBool, FfiChildOf, FfiEntity, FfiOption,
    FfiResult, FfiSlice, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent, RawInsertable,
    ScheduleId, StrView, VTableHeader,
};

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::{AbiPod, ModComponent};

#[cfg(feature = "derive")]
pub use crate::bridges::{
    GuestGlobalTransformBridge, GuestKeyboardInputBridge, GuestMouseInputBridge, GuestStdTypes,
    GuestTimeFixedBridge, GuestTimeRealBridge, GuestTimeVirtualBridge, GuestTransformBridge,
    GuestVisibilityBridge, GuestWireBridge,
};
pub use crate::{
    app::{
        GuestApp, GuestComponentOptions, GuestRes, GuestResMut, ObserverBuilder, QueryBuilder,
        RelationDescBuilder, RunConditionContext, ScheduleSetBuilder, SystemBuilder, SystemContext,
    },
    bridges::{
        BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
        BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
        FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput,
        FfiTimeFixed, FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, GuestAssetHandle,
        GuestAssetHostExt, GuestAssetServer, GuestBridgesHostExt, GuestCommandsBsnExt,
        GuestCommandsStateExt, GuestNameBridge, GuestScene, GuestStateBridge, GuestStateHandles,
        GuestStateQueryBuilderExt, GuestStateSystemBuilderExt, GuestStdCommandsExt,
        GuestStdHandles, GuestStdQueryBuilderExt, TypedAssetHandle, on_enter_schedule,
        on_exit_schedule, on_transition_schedule, resolve_reflected_handle,
        resolve_scene_patch_type, resolve_settings_handle,
    },
    commands::{GuestCommands, GuestTransaction},
    component::{GuestComponent, GuestComponentHooks, guest_component_uuid},
    export_mod, guest_log,
    iter::{
        Added, Allow, Changed, Chunk, ChunkIter, ChunkSlice, ChunkSliceMut, Has, Or, Or2, Or3,
        OrFilterTerm, OrFilterTuple, QueryTerm, QueryTuple, SparseRows, SparseRowsMut,
        TypedChunkIter, With, Without,
    },
    lifecycle::GuestPlugin,
};
