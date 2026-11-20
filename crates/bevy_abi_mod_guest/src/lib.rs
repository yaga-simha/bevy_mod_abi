#![forbid(unsafe_code)]
#![warn(missing_docs)]

//! # `bevy_abi_mod_guest`: Safe Idiomatic Rust Guest SDK (`ABI 0.3.0`)
//!
//! Zero-Bevy-dependency safe Rust SDK for authoring native `.cdylib` mods targeting
//! `bevy_abi_mod` (`ABI 0.3.0`).
//!
//! Every unsafe FFI operation, pointer check, [`VTableHeader`] layout verification, and `extern "C"`
//! panic trampoline is isolated inside `bevy_abi_mod_guest_sys`; this crate enforces
//! `#![forbid(unsafe_code)]` unconditionally.
//!
//! ## Core Capabilities
//! - **Per-World Instance Isolation:** [`export_mod!`] exports
//!   `bevy_abi_mod_manifest`, `bevy_abi_mod_init`, and `bevy_abi_mod_shutdown` backed by a
//!   heap-allocated per-World [`GuestInstance`](raw::GuestInstance) with **zero** process-global
//!   mutable state—loading the same `.cdylib` into multiple Bevy `World`s simultaneously is
//!   completely isolated.
//! - **Unified `app.system(...)` Builder & Typed Queries:** [`GuestApp::system`] compiles up to
//!   [`MAX_QUERIES_PER_SYSTEM`] typed tuple queries (`query_typed::<(&mut A, &B, With<C>, Without<D>, Allow<E>, Has<F>, Or<(With<G>, Added<H>)>)>()`
//!   up to 16 query terms, plus [`Or2`] / [`Or3`] aliases),
//!   `RemovedComponents<T>` cursors ([`QueryBuilder::removed`], [`SystemContext::read_removed`]), declaration-indexed singleton
//!   resources ([`GuestRes`] / [`GuestResMut`] with [`BamChangeTicks`]), [`ScheduleId`] selection,
//!   `.after(...)` / `.before(...)` / `.after_weak(...)` / `.in_set(...)` / `.ambiguous_with(...)`
//!   ordering (including `"host::<name>"` sets), decoupled [`SystemBuilder::register_handle`] +
//!   [`ScheduleSetBuilder`] + [`GuestApp::mount_system`], `.main_thread()` pinning, and lock-free
//!   `.run_if(...)` predicates into a single [`BamSystemDesc`](raw::BamSystemDesc).
//! - **Verified Zero-Copy Views & Parallelism:** [`Chunk`] validates [`BamColumnInfo`]
//!   (`size`, `align`, `mode`, storage flags, and `ComponentUuid` via [`Chunk::component`]),
//!   provides indirect sparse-set row views ([`Chunk::rows`] / [`Chunk::rows_mut`]), supports
//!   untracked mutable slices paired with [`Chunk::mark_changed_range`], non-destructive
//!   [`ChunkIter::count`] / [`ChunkIter::up_cascade`] ancestor lookups, and offers work-stealing
//!   [`ChunkIter::par_for_each`] / [`ChunkIter::par_for_each_chunk_readonly`] (and
//!   [`SystemContext::par_chunks`] / [`SystemContext::par_chunks_readonly`]) iteration.
//! - **Deferred Commands, Transactions, Relationships, Messages & Observers:** [`GuestCommands`]
//!   enqueues zero-steady-state-allocation spawns (`spawn_empty`, `spawn`, `spawn_persistent`,
//!   `spawn_batch`, [`FfiChildOf`]), `despawn`, `despawn_all`, `insert`, `insert_if_new`,
//!   `insert_default`, `remove`, `trigger`, `trigger_ex`, typed relationships
//!   ([`RelationDescBuilder`], [`GuestApp::register_relation`], [`GuestCommands::insert_relation`],
//!   [`GuestCommands::remove_relation`]), and RAII command transactions
//!   ([`GuestCommands::transaction`], [`GuestTransaction`]), alongside batch `Messages<M>`
//!   read/write (POD and witness-backed non-POD) and dynamic global, `EntityEvent`, lifecycle,
//!   `Ready`, mutable, and upward-propagating observers ([`GuestApp::observe`],
//!   [`ObserverBuilder`]).
//! - **Optional Subsystem Bridges (`bridges`):** Ergonomic guest wrappers for Bevy standard
//!   components/inputs (`std_types`), isolated `mod://<ns>/...` assets (`GuestAssetServer`),
//!   `States` (`GuestStateBridge`), reflection/BRP/settings (`bridges::reflect`), and BSN scenes
//!   (`GuestScene`).
//!
//! ## Quickstart
//!
//! ```no_run
//! use bevy_abi_mod_guest::prelude::*;
//! use core::ffi::CStr;
//!
//! #[derive(Default)]
//! struct HierarchyMod;
//!
//! impl GuestPlugin for HierarchyMod {
//!     const NAME: &'static CStr = c"hierarchy_mod";
//!     const VERSION: &'static CStr = c"0.2.0";
//!
//!     fn build(&mut self, app: &mut GuestApp<'_>) -> FfiResult {
//!         app.system(c"inspect_parents")
//!             .query(|q| q.read::<FfiChildOf>())
//!             .schedule(ScheduleId::UPDATE)
//!             .build(|ctx| {
//!                 while let Some(chunk) = ctx.iter().next_chunk_view() {
//!                     let _parents = chunk.component::<FfiChildOf>(0);
//!                 }
//!             })
//!     }
//! }
//!
//! export_mod!(plugin: HierarchyMod);
//! ```
//!
//! ## Panic & Unwind Policy: `panic = "unwind"` vs. `panic = "abort"`
//!
//! Every function pointer across `bevy_abi_mod` uses `extern "C"` (`nounwind`), never
//! `extern "C-unwind"`. Unwinding across an `extern "C"` boundary is Undefined Behavior, and the
//! host never catches guest unwinds because freeing a guest-allocated `Box<dyn Any + Send>` panic
//! payload on the host allocator would corrupt the heap.
//!
//! - **`panic = "unwind"` *(Cargo default — recommended)*:** Every guest trampoline (`bevy_abi_mod_init`,
//!   `bevy_abi_mod_shutdown`, system callbacks, `run_if` predicates, and component destructors)
//!   catches panics **inside the guest runtime**, drops the panic payload on the **guest** heap,
//!   releases its lock-free re-entrancy guard, and returns [`FfiResult::PANIC_CAUGHT`] (or
//!   [`FfiBool::FALSE`] for `run_if`). The host discards uncommitted commands from that tick and
//!   applies `ModErrorPolicy` (e.g. quarantining the faulting system) while keeping the host
//!   application running.
//! - **`panic = "abort"`:** `std::panic::catch_unwind` compiles to a zero-overhead direct call, and
//!   any panic inside the guest immediately terminates the OS process via `abort()` without
//!   crossing `extern "C"`. Use `panic = "abort"` only for trusted first-party mods when smaller
//!   binary size and fail-fast termination are desired.

pub use bevy_abi_mod_sys;

pub mod app;
pub mod bridges;
pub mod commands;
pub mod commands_lifecycle;
pub mod component;
pub mod iter;
pub mod lifecycle;
pub mod macros;
pub mod prelude;
pub mod raw;

pub use bevy_abi_mod_guest_sys::SpawnEntry;
pub use bevy_abi_mod_sys::{
    ABI_VERSION, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH, AbiPod, AccessMode,
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK,
    BAM_LOG_DEBUG, BAM_LOG_ERROR, BAM_LOG_INFO, BAM_LOG_TRACE, BAM_LOG_WARN,
    BAM_OBSERVER_FLAG_PROPAGATE, BAM_RELATION_FLAG_ACYCLIC, BAM_RELATION_FLAG_CASCADE_DELETE,
    BAM_RELATION_FLAG_CASCADE_DESPAWN, BAM_RELATION_FLAG_CLEANUP_REMOVE,
    BAM_RELATION_FLAG_EXCLUSIVE, BAM_RELATION_FLAG_SYMMETRIC, BAM_SCHEDULE_SET_FLAG_AFTER_WEAK,
    BAM_SCHEDULE_SET_FLAG_BEFORE_WEAK, BAM_SCHEDULE_SET_FLAG_PARALLEL,
    BAM_SCHEDULE_SET_FLAG_SEQUENCE, BAM_SCHEDULE_SET_FLAG_SEQUENCE_WEAK,
    BAM_STORAGE_FLAG_IMMUTABLE, BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE,
    BAM_STORAGE_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_SUMMARY_TICK, BAM_SYSTEM_FLAG_AMBIGUOUS_WITH_ALL,
    BAM_SYSTEM_FLAG_MAIN_THREAD, BAM_TRIGGER_FLAG_PROPAGATE, BamChangeTicks, BamColumnInfo,
    BamComponent, BamEventKind, BamFieldDesc, BamFieldKind, BamLayoutInfo, BamManifest,
    BamObserverDesc, BamObserverFn, BamObserverMutFn, BamRelationDesc, BamRequiredComponent,
    BamRowMigrateFn, BamScheduleSetDesc, BamSymbol, BamSystem, BamSystemSet, BamTermDesc,
    BamTriggerDesc, ComponentDesc, ComponentUuid, FfiAssetHandle, FfiAssetLoadState, FfiBool,
    FfiChildOf, FfiEntity, FfiOption, FfiResult, FfiSlice, MAX_QUERIES_PER_SYSTEM,
    MAX_TERMS_PER_QUERY, ModComponent, RawInsertable, ScheduleId, StrView, SymbolEntry,
    VTableHeader,
};

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::{AbiPod, GuestComponent, ModComponent};

pub use app::{
    GuestApp, GuestComponentOptions, GuestRes, GuestResMut, IntoSystemResult, ObserverBuilder,
    QueryBuilder, RelationDescBuilder, RunConditionContext, ScheduleSetBuilder, SystemBuilder,
    SystemContext,
};
pub use bridges::{
    BRP_METHOD_GET_COMPONENTS, BRP_METHOD_GET_RESOURCES, BRP_METHOD_INSERT_COMPONENTS,
    BRP_METHOD_LIST_COMPONENTS, BRP_METHOD_MUTATE_COMPONENTS, BRP_METHOD_MUTATE_RESOURCES,
    FfiGlobalTransform, FfiKeyCode, FfiKeyboardInput, FfiMouseButton, FfiMouseInput, FfiTimeFixed,
    FfiTimeReal, FfiTimeVirtual, FfiTransform, FfiVisibility, GuestAssetHandle, GuestAssetHostExt,
    GuestAssetServer, GuestBridgesHostExt, GuestCommandsBsnExt, GuestCommandsStateExt,
    GuestNameBridge, GuestScene, GuestStateBridge, GuestStateHandles, GuestStateQueryBuilderExt,
    GuestStateSystemBuilderExt, GuestStdCommandsExt, GuestStdHandles, GuestStdQueryBuilderExt,
    TypedAssetHandle, on_enter_schedule, on_exit_schedule, on_transition_schedule,
    resolve_reflected_handle, resolve_scene_patch_type, resolve_settings_handle,
};
#[cfg(feature = "derive")]
pub use bridges::{
    GuestGlobalTransformBridge, GuestKeyboardInputBridge, GuestMouseInputBridge, GuestStdTypes,
    GuestTimeFixedBridge, GuestTimeRealBridge, GuestTimeVirtualBridge, GuestTransformBridge,
    GuestVisibilityBridge, GuestWireBridge,
};
pub use commands::{GuestCommands, GuestTransaction};
pub use component::{GuestComponent, GuestComponentHooks, guest_component_uuid};
pub use iter::{
    Added, Allow, Changed, Chunk, ChunkIter, ChunkSlice, ChunkSliceMut, Has, Or, Or2, Or3,
    OrFilterTerm, OrFilterTuple, QueryTerm, QueryTuple, SparseRows, SparseRowsIter, SparseRowsMut,
    TypedChunkIter, With, Without,
};
pub use lifecycle::{GuestPlugin, build_guest_fn, build_guest_plugin};

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_guest_sys::ValidatedSlice;

    // Verify `guest_component_uuid` is const-evaluatable over `AbiPod` array types unconditionally.
    const _: () = {
        let u = guest_component_uuid::<[f32; 2]>("game::Pos", 0);
        assert!(!u.is_zero());
    };

    #[cfg(feature = "derive")]
    mod derive_smoke {
        use crate::prelude::*;

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq, AbiPod, GuestComponent)]
        #[guest_component(name = "demo::DerivedVec2", no_schema_hash)]
        struct DerivedVec2 {
            x: f32,
            y: f32,
        }

        #[repr(C)]
        #[derive(Clone, Copy, Debug, PartialEq, ModComponent)]
        #[mod_component(name = "demo::ShorthandVec2", summary_tick, guest)]
        struct ShorthandVec2 {
            x: f32,
            y: f32,
        }

        #[test]
        fn derived_guest_component_uuid_is_deterministic() {
            let expected = ComponentUuid::from_parts(
                "demo::DerivedVec2",
                core::mem::size_of::<DerivedVec2>(),
                core::mem::align_of::<DerivedVec2>(),
                0,
            );
            assert_eq!(<DerivedVec2 as GuestComponent>::UUID, expected);
            assert_eq!(
                <ShorthandVec2 as GuestComponent>::UUID,
                <ShorthandVec2 as ModComponent>::UUID
            );
            assert_eq!(
                <ShorthandVec2 as GuestComponent>::FLAGS,
                BAM_COMPONENT_FLAG_SUMMARY_TICK
            );
        }
    }

    #[test]
    fn invalid_slices_return_none_safely() {
        let slice = ChunkSlice::from_validated(ValidatedSlice::invalid());
        assert!(slice.is_empty());
        assert_eq!(slice.len(), 0);
        assert!(slice.as_typed_slice::<f32>().is_none());

        let mut mut_slice =
            ChunkSliceMut::from_validated(bevy_abi_mod_guest_sys::ValidatedSliceMut::invalid());
        assert!(mut_slice.is_empty());
        assert!(mut_slice.as_typed_slice::<f32>().is_none());
        assert!(mut_slice.as_typed_slice_mut::<f32>().is_none());
    }
}
