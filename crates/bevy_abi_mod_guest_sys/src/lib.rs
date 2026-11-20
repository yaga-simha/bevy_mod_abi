//! Low-level `unsafe` FFI internals for `bevy_abi_mod` Rust guest modules (`ABI 0.3.0`).
//!
//! Isolates every guest-side `unsafe` block behind documented `# Safety` contracts:
//! - Per-instance [`Host`] API table copy, SemVer validation, `"bam.v0.*"` custom symbol lookup,
//!   and [`VTableHeader`](bevy_abi_mod_sys::VTableHeader) verification.
//! - Per-World [`GuestInstance`] closure/descriptor ownership and LIFO teardown with zero
//!   process-global mutable state.
//! - Lock-free [`ReentrantCell`] (`UnsafeCell` + `AtomicBool` `Acquire`/`Release` guard) and
//!   panic-catching `extern "C"` trampolines that drop unwind payloads on the guest heap.
//! - Validated dense column slices, indirect sparse-set row views, untracked mutation with
//!   `mark_changed_range`, declaration-indexed singleton resources, and handle/batched commands.
//!
//! Mod authors should depend on `bevy_abi_mod_guest` (`#![forbid(unsafe_code)]`) rather than
//! calling this `-sys` crate directly.

#![warn(missing_docs)]
#![deny(
    unsafe_op_in_unsafe_fn,
    clippy::undocumented_unsafe_blocks,
    clippy::missing_safety_doc
)]

/// Safely resolves an optional extended [`bevy_abi_mod_sys::BamHostApi`] slot, returning
/// `Some(fn_ptr)` only when `api.covers_slot(offset_of!(BamHostApi, $slot), size_of_val(&api.$slot))`
/// and the slot is populated.
macro_rules! host_slot {
    ($api:expr, $slot:ident) => {{
        let __api: &::bevy_abi_mod_sys::BamHostApi = $api;
        if __api.covers_slot(
            ::core::mem::offset_of!(::bevy_abi_mod_sys::BamHostApi, $slot),
            ::core::mem::size_of_val(&__api.$slot),
        ) {
            __api.$slot
        } else {
            ::core::option::Option::None
        }
    }};
}

pub mod commands;
pub mod component;
pub mod host;
pub mod instance;
pub mod iter;
pub mod panic;
pub mod trampoline;

pub use bevy_abi_mod_sys::{BamSystemCtx, FfiBool, FfiResult};

pub use commands::{SpawnBatchColumn, SpawnEntry, ValidatedCommands};
pub use component::{
    PodComponentHooks, generic_drop_trampoline, migrate_component_column_pod,
    migrate_component_column_raw, migrate_mod_component_column, migrate_pod_component_column,
    pod_clone_trampoline, register_component_raw, register_component_with_layout,
    register_dynamic_component_with_drop, register_pod_component,
    register_pod_component_with_hooks, register_relation, register_relation_raw, resolve_component,
};
pub use host::{
    CUSTOM_SYMBOL_PREFIX, Host, ValidatedLoadCtx, ValidatedStringVTable, ValidatedWireVTable,
};
pub use instance::{
    GuestInstance, guest_restore_state_trampoline, guest_save_state_trampoline,
    init_guest_instance, qualify_mod_component_identity, resolve_guest_instance_symbol,
    resolve_mod_component, shutdown_guest_instance,
};
pub use iter::{
    ChunkColumnDispenser, SparseRows, SparseRowsIter, SparseRowsMut, UuidFilter, ValidatedIter,
    ValidatedSlice, ValidatedSliceMut,
};
pub use panic::{catch_guest_bool, catch_guest_result, catch_guest_unit};
pub use trampoline::{
    PreparedRunIf, ReentrantCell, SystemRegistrationConfig, ValidatedSystemInvocation,
    ValidatedSystemRef, allocate_run_if_trampoline, allocate_system_trampoline,
    configure_schedule_set, configure_schedule_set_raw, iter_dynamic_slice, iter_dynamic_slice_mut,
    iter_entities_and_dynamic_slice_mut, mount_system_in_schedule,
    register_lifecycle_observer_closure, register_observer_closure,
    register_observer_closure_with_options, register_observer_mut_closure, register_system_closure,
    register_system_handle_closure, verify_same_mod_dynamic_component,
};
