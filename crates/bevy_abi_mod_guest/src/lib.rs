#![forbid(unsafe_code)]

//! # `bevy_abi_mod_guest`: Safe Idiomatic Rust Guest SDK (`ABI 0.2.0`)
//!
//! Zero-Bevy-dependency safe Rust SDK for authoring native `.cdylib` mods targeting
//! `bevy_abi_mod` (`ABI 0.2.0`).
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
//! - **Unified `app.system(...)` Builder:** [`GuestApp::system`] compiles up to
//!   [`MAX_QUERIES_PER_SYSTEM`] queries, declaration-indexed singleton resources, [`ScheduleId`]
//!   selection, `.after(...)` / `.before(...)` named ordering, `.main_thread()` pinning, and
//!   lock-free `.run_if(...)` predicates into a single [`BamSystemDesc`](raw::BamSystemDesc).
//! - **Verified Zero-Copy Views:** [`Chunk`] validates [`BamColumnInfo`]
//!   (`size`, `align`, `mode`, storage flags, and `ComponentUuid` via [`Chunk::component`]),
//!   provides indirect sparse-set row views ([`Chunk::rows`] / [`Chunk::rows_mut`]), and supports
//!   untracked mutable slices paired with [`Chunk::mark_changed_range`].
//! - **Handle-Based & Batched Commands:** [`GuestCommands`] enqueues zero-steady-state-allocation
//!   deferred spawns (`spawn_empty`, single-archetype batched `spawn`), `despawn`, `insert`,
//!   `insert_default`, and `remove` using 1-based [`BamComponent`] handles.
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
pub mod commands;
pub mod component;
pub mod iter;
pub mod lifecycle;
pub mod macros;
pub mod prelude;
pub mod raw;

pub use bevy_abi_mod_guest_sys::SpawnEntry;
pub use bevy_abi_mod_sys::{
    ABI_VERSION, ABI_VERSION_MAJOR, ABI_VERSION_MINOR, ABI_VERSION_PATCH, AbiPod, AccessMode,
    BAM_COMPONENT_FLAG_IMMUTABLE, BAM_COMPONENT_FLAG_SPARSE_SET, BAM_STORAGE_FLAG_IMMUTABLE,
    BAM_STORAGE_FLAG_MESSAGE, BAM_STORAGE_FLAG_RESOURCE, BAM_STORAGE_FLAG_SPARSE_SET,
    BAM_SYSTEM_FLAG_MAIN_THREAD, BamChangeTicks, BamColumnInfo, BamComponent, BamManifest,
    BamObserverDesc, BamObserverFn, BamTermDesc, ComponentDesc, ComponentUuid, FfiBool, FfiChildOf,
    FfiEntity, FfiResult, FfiSlice, MAX_QUERIES_PER_SYSTEM, MAX_TERMS_PER_QUERY, ModComponent,
    RawInsertable, ScheduleId, StrView, SymbolEntry, VTableHeader,
};

#[cfg(feature = "derive")]
pub use bevy_abi_mod_macros::{AbiPod, ModComponent};

pub use app::{
    GuestApp, GuestRes, GuestResMut, IntoSystemResult, MultiQueryContext, QueryBuilder,
    RunConditionContext, SystemBuilder, SystemContext,
};
pub use commands::GuestCommands;
pub use component::{GuestComponent, GuestComponentHooks, guest_component_uuid};
pub use iter::{
    Added, Changed, Chunk, ChunkIter, ChunkSlice, ChunkSliceMut, QueryTerm, QueryTuple, SparseRows,
    SparseRowsIter, SparseRowsMut, TypedChunkIter, With, Without,
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
        #[derive(Clone, Copy, Debug, PartialEq, AbiPod)]
        struct DerivedVec2 {
            x: f32,
            y: f32,
        }

        impl GuestComponent for DerivedVec2 {
            const NAME: &'static core::ffi::CStr = c"demo::DerivedVec2";
        }

        #[test]
        fn derived_guest_component_uuid_is_deterministic() {
            let expected = ComponentUuid::from_parts(
                "demo::DerivedVec2",
                core::mem::size_of::<DerivedVec2>(),
                core::mem::align_of::<DerivedVec2>(),
                0,
            );
            assert_eq!(DerivedVec2::UUID, expected);
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
