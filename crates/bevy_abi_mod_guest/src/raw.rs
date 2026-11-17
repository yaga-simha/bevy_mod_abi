//! Raw ABI `0.2` descriptors, `BamHostApi` table, and low-level `bevy_abi_mod_guest_sys` escape
//! hatches (`G-01`).
//!
//! Exposes the entirety of [`bevy_abi_mod_sys`] (`bevy_abi_mod_guest::raw::*`) alongside
//! `bevy_abi_mod_guest_sys` validation/lifecycle helpers for VTable callers, custom `"bam.v0.*"`
//! symbol consumers, and raw C-ABI tests.

pub use bevy_abi_mod_sys::*;

pub use bevy_abi_mod_guest_sys::{
    CUSTOM_SYMBOL_PREFIX, ChunkColumnDispenser, GuestInstance, Host, PodComponentHooks,
    PreparedRunIf, ReentrantCell, SparseRows, SparseRowsIter, SparseRowsMut, SpawnEntry,
    SystemRegistrationConfig, UuidFilter, ValidatedCommands, ValidatedIter, ValidatedLoadCtx,
    ValidatedSlice, ValidatedSliceMut, ValidatedSystemInvocation, ValidatedSystemRef,
    allocate_run_if_trampoline, allocate_system_trampoline, catch_guest_bool, catch_guest_result,
    catch_guest_unit, generic_drop_trampoline, guest_restore_state_trampoline,
    guest_save_state_trampoline, init_guest_instance, register_component_raw,
    register_dynamic_component_with_drop, register_pod_component,
    register_pod_component_with_hooks, register_system_closure, resolve_component,
    shutdown_guest_instance, wrap_run_condition, wrap_system_callback,
};

use crate::{
    app::{GuestApp, SystemContext},
    commands::GuestCommands,
    iter::{ChunkIter, ChunkSlice, ChunkSliceMut},
};

/// Raw pointer inspection helpers on [`GuestApp`].
pub trait GuestAppRawExt {
    /// Returns the raw `*mut BamLoadCtx` pointer for this init session.
    fn raw_load_ctx(&self) -> *mut BamLoadCtx;
    /// Returns the world-scoped `*const BamHostCtx` token.
    fn raw_host_ctx(&self) -> *const BamHostCtx;
}

impl GuestAppRawExt for GuestApp<'_> {
    #[inline]
    fn raw_load_ctx(&self) -> *mut BamLoadCtx {
        self.as_raw_load_ctx()
    }

    #[inline]
    fn raw_host_ctx(&self) -> *const BamHostCtx {
        self.host().host_ctx()
    }
}

/// Raw pointer inspection helpers on [`SystemContext`].
pub trait SystemContextRawExt {
    /// Returns the raw `*mut BamSystemCtx` pointer for this tick.
    fn raw_sys_ctx(&self) -> *mut BamSystemCtx;
    /// Returns the world-scoped `*const BamHostCtx` token.
    fn raw_host_ctx(&self) -> *const BamHostCtx;
}

impl SystemContextRawExt for SystemContext<'_> {
    #[inline]
    fn raw_sys_ctx(&self) -> *mut BamSystemCtx {
        self.as_raw_ptr()
    }

    #[inline]
    fn raw_host_ctx(&self) -> *const BamHostCtx {
        self.host().host_ctx()
    }
}

/// Raw pointer inspection helpers on [`ChunkIter`].
pub trait ChunkIterRawExt {
    /// Returns the raw `*mut BamIter` pointer.
    fn raw_iter(&self) -> *mut BamIter;
}

impl ChunkIterRawExt for ChunkIter<'_> {
    #[inline]
    fn raw_iter(&self) -> *mut BamIter {
        self.as_raw_ptr()
    }
}

/// Raw pointer inspection helpers on [`GuestCommands`].
pub trait GuestCommandsRawExt {
    /// Returns the raw `*mut BamCommands` pointer.
    fn raw_commands(&self) -> *mut BamCommands;
}

impl GuestCommandsRawExt for GuestCommands<'_> {
    #[inline]
    fn raw_commands(&self) -> *mut BamCommands {
        self.as_raw_ptr()
    }
}

/// Raw pointer inspection helpers on [`ChunkSlice`].
pub trait ChunkSliceRawExt {
    /// Returns the raw `*const u8` column base pointer.
    fn raw_ptr(&self) -> *const u8;
}

impl ChunkSliceRawExt for ChunkSlice<'_> {
    #[inline]
    fn raw_ptr(&self) -> *const u8 {
        self.as_ptr()
    }
}

/// Raw pointer inspection helpers on [`ChunkSliceMut`].
pub trait ChunkSliceMutRawExt {
    /// Returns the raw `*const u8` column base pointer.
    fn raw_ptr(&self) -> *const u8;
    /// Returns the raw `*mut u8` mutable column base pointer.
    fn raw_mut_ptr(&mut self) -> *mut u8;
}

impl ChunkSliceMutRawExt for ChunkSliceMut<'_> {
    #[inline]
    fn raw_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn raw_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }
}
