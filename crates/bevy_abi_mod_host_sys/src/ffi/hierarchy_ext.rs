//! Hierarchy and `RelationshipTarget` inspection entrypoint (`sys_entity_children`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{BamComponent, BamSystemCtx, FfiEntity, FfiResult};

use crate::{
    handle_header::{SYS_CTX_MAGIC, validate_handle_ref},
    system::HostSystemCtx,
};

/// Borrows the contiguous slice of child or custom `RelationshipTarget` entities for `entity`,
/// writing the zero-copy pointer and length into `*out_entities` and `*out_len`.
///
/// # Safety
/// - `sys_ctx` must point to an active [`HostSystemCtx`] cast to [`BamSystemCtx`], or null.
/// - `out_entities` must point to writable `*const FfiEntity` storage, or null.
/// - `out_len` must point to writable `usize` storage, or null.
pub unsafe extern "C" fn host_sys_entity_children(
    sys_ctx: *const BamSystemCtx,
    entity: FfiEntity,
    _relationship_target: BamComponent,
    out_entities: *mut *const FfiEntity,
    out_len: *mut usize,
) -> FfiResult {
    if out_entities.is_null() || out_len.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if !out_entities.is_aligned() || !out_len.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_entities` and `out_len` are non-null, aligned, and writable.
    unsafe {
        *out_entities = core::ptr::null();
        *out_len = 0;
    }
    // SAFETY: Validates `sys_ctx` against `SYS_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(_ctx) = (unsafe {
        validate_handle_ref::<HostSystemCtx<'_>>(sys_ctx.cast::<c_void>(), SYS_CTX_MAGIC, false)
    }) else {
        return if sys_ctx.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    if entity.is_null() {
        return FfiResult::INVALID_ENTITY;
    }
    FfiResult::UNSUPPORTED
}
