#![no_std]
#![warn(missing_docs)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::missing_safety_doc)]

//! # `bevy_abi_mod_sys`: Zero-Dependency C-ABI `0.2` Wire Contract for `bevy_abi_mod`
//!
//! This crate defines the `#![no_std]`, zero-dependency C-ABI `0.2.0` wire specification connecting
//! Bevy ECS host applications to native dynamic guest mods (`.so`, `.dylib`, `.dll`) written in
//! Rust, C99, C++, or Zig.
//!
//! ## Core Architectural Invariants
//!
//! 1. **Zero Shared Rust ABI Assumptions:** Every wire struct is `#[repr(C)]` or
//!    `#[repr(transparent)]`, every foreign callback slot is `Option<unsafe extern "C" fn(...)>`,
//!    and arrays/strings cross the boundary via [`FfiSlice`] / [`StrView`] or NUL-terminated C
//!    strings rather than Rust fat pointers.
//! 2. **Strict Memory & Allocator Isolation:** Host-allocated memory is freed exclusively by the
//!    host, and guest-allocated memory (such as per-World [`BamInstance`] state) is freed
//!    exclusively by the guest in `bevy_abi_mod_shutdown`.
//! 3. **Pre-Resolved Handles & Zero-Allocation Hot Paths:** Components and resources resolve once
//!    at load time into 1-based [`BamComponent`] handles; hot-path commands and declaration-indexed
//!    resource accessors perform zero string parsing or hashing.
//! 4. **Nounwind `extern "C"` Boundary:** Unwinding across `extern "C"` is forbidden. Unwinding
//!    Rust guests catch panics inside their own runtime and return [`FfiResult::PANIC_CAUGHT`].
//!
//! ## Why `#![no_std]` and Zero External Dependencies?
//!
//! Keeping `bevy_abi_mod_sys` strictly `#![no_std]` with zero external dependencies (no `bevy`,
//! no `alloc`, no `std`) guarantees that guest mods, proc-macro expansions, and the host engine
//! share the exact same wire types without pulling in Bevy's ECS internals or coupling to any
//! specific heap allocator.

mod access;
mod commands;
mod component;
mod extensible;
mod host_api;
mod iter;
mod primitives;
mod system;
mod traits;
mod version;

#[cfg(test)]
mod tests;

pub use access::{
    AccessMode, BAM_TERM_KNOWN_FLAGS, BamTermDesc, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY,
    ScheduleId,
};
pub use commands::{
    BAM_TRIGGER_FLAG_PROPAGATE, BAM_TRIGGER_KNOWN_FLAGS, BamCmdDespawnAllFn, BamCmdDespawnFn,
    BamCmdInsertDefaultFn, BamCmdInsertFn, BamCmdObserveFn, BamCmdRemoveFn, BamCmdSpawnBatchFn,
    BamCmdSpawnEmptyFn, BamCmdSpawnFn, BamCmdSpawnSceneFn, BamCmdTriggerExFn, BamCmdTriggerFn,
    BamCommands, BamTriggerDesc,
};
pub use component::{
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK,
    BAM_COMPONENT_KNOWN_FLAGS, ComponentCloneFn, ComponentDesc, ComponentDropFn, ComponentHookFn,
    MappedStringGetFn, MappedStringSetFn, MappedStringVTable, MappedWireGetFn, MappedWireSetFn,
    MappedWireVTable, VTableHeader,
};
pub use extensible::{ExtensibleStruct, read_descriptor, read_extensible_struct};
pub use host_api::{
    BAM_MANIFEST_KNOWN_FLAGS, BamAssetGetLoadStateFn, BamAssetLoadFn, BamAssetReleaseFn,
    BamGetProcAddressFn, BamHostApi, BamInitFn, BamInstance, BamLastErrorFn, BamLoadHostCtxFn,
    BamManifest, BamManifestFn, BamRegisterComponentFn, BamRegisterObserverFn, BamRegisterSystemFn,
    BamResolveComponentFn, BamRestoreStateFn, BamSaveStateFn, BamShutdownFn, BamStateWriterFn,
    BamSysCommandsFn, BamSysEntityChildrenFn, BamSysEntityGetFn, BamSysEntityGetMutFn,
    BamSysHostCtxFn, BamSysQueryFn, BamSysResMutPtrFn, BamSysResPtrFn,
};
pub use iter::{
    BAM_MAX_CHANGE_AGE, BAM_STORAGE_FLAG_ENTITY_EVENT, BAM_STORAGE_FLAG_EVENT,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_SUMMARY_TICK, BamChangeTicks, BamChunkTaskFn,
    BamColumnInfo, BamIter, BamIterColumnInfoFn, BamIterColumnMutableFn,
    BamIterColumnMutableUntrackedFn, BamIterColumnReadonlyFn, BamIterColumnRowsMutableFn,
    BamIterColumnRowsReadonlyFn, BamIterEntitiesFn, BamIterMarkChangedRangeFn, BamIterNextChunkFn,
    BamIterReadRemovedFn, BamIterResetFn, BamMsgReadBatchFn, BamMsgSendFn, BamMsgWriteBatchFn,
    BamParForEachChunkFn, BamResMutPtrFn, BamResPtrFn, BamResTicksFn,
};
pub use primitives::{
    BAM_CHILD_OF_NAME, BAM_CHILDREN_NAME, BamComponent, ComponentUuid, FfiAssetHandle,
    FfiAssetLoadState, FfiBool, FfiChildOf, FfiEntity, FfiOption, FfiResult, FfiSlice,
    SIPHASH_KEY_0, SIPHASH_KEY_1, StrView, siphash24_128,
};
pub use system::{
    BAM_OBSERVER_FLAG_PROPAGATE, BAM_OBSERVER_KNOWN_FLAGS, BAM_QUERY_KNOWN_FLAGS,
    BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL, BAM_SYSTEM_FLAG_MAIN_THREAD, BAM_SYSTEM_KNOWN_FLAGS,
    BamEventKind, BamHostCtx, BamLoadCtx, BamObserverDesc, BamObserverFn, BamObserverMutFn,
    BamQueryDesc, BamRunIfFn, BamSystemCtx, BamSystemDesc, BamSystemFn,
};
pub use traits::{
    AbiPod, ModComponent, RawInsertable, SymbolEntry, assert_abi_pod, assert_raw_insertable,
};
pub use version::{
    ABI_VERSION, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH,
    abi_version_is_compatible, abi_version_major, abi_version_minor, abi_version_patch,
    pack_abi_version, unpack_abi_version,
};
