//! Lifecycle `RemovedComponents<T>` batch reader (`iter_read_removed`) entrypoint.

use core::ffi::c_void;

use bevy_abi_mod_sys::{AccessMode, BamIter, FfiEntity, FfiResult};
use bevy_ecs::entity::Entity;
use bevy_ecs::lifecycle::RemovedComponentEntity;
use bevy_ecs::message::MessageCursor;

use crate::{
    ffi::iter::HostIter,
    handle_header::{ITER_MAGIC, validate_handle_mut},
};

/// Reads up to `max_count` removed-component [`FfiEntity`] values for `term_index` (`REMOVED_READ`).
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `out_read` must point to writable `usize` storage, or null.
/// - When `max_count > 0`, `out_entities` must point to `max_count` writable [`FfiEntity`] slots.
pub unsafe extern "C" fn host_iter_read_removed(
    iter: *mut BamIter,
    term_index: usize,
    out_entities: *mut FfiEntity,
    max_count: usize,
    out_read: *mut usize,
) -> FfiResult {
    if out_read.is_null() || (max_count > 0 && out_entities.is_null()) {
        return FfiResult::NULL_POINTER;
    }
    if !out_read.is_aligned() || (max_count > 0 && !out_entities.is_aligned()) {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_read` is non-null, aligned, and writable.
    unsafe {
        *out_read = 0;
    }
    // SAFETY: Validates `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = match unsafe {
        validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false)
    } {
        Ok(it) => it,
        Err(err) => return err,
    };
    if host_iter.in_par_iter {
        return FfiResult::ACCESS_CONFLICT;
    }
    if term_index >= host_iter.plan.terms.len() {
        return FfiResult::OUT_OF_RANGE;
    }
    let term = &host_iter.plan.terms[term_index];
    if term.mode != AccessMode::REMOVED_READ || term.is_resource() || term.is_message() {
        return FfiResult::PERMISSION_DENIED;
    }
    if max_count == 0 {
        return FfiResult::OK;
    }
    let Some(world) = host_iter.world else {
        return FfiResult::INVALID_STATE;
    };
    let Some(ref mut cursors) = host_iter.msg_cursors else {
        return FfiResult::INVALID_STATE;
    };
    let Some(Some(cursor_box)) = cursors.get_mut(term_index) else {
        return FfiResult::INVALID_STATE;
    };
    let Some(cursor) = cursor_box.downcast_mut::<MessageCursor<RemovedComponentEntity>>() else {
        return FfiResult::INVALID_STATE;
    };
    let Some(messages) = world.removed_components().get(term.component_id) else {
        return FfiResult::OK;
    };

    let mut written = 0usize;
    for removed in cursor.read(messages).take(max_count) {
        let entity: Entity = removed.clone().into();
        // SAFETY: `out_entities` is non-null, aligned, and caller guarantees `max_count` writable
        // `FfiEntity` slots while `written < max_count`.
        unsafe {
            core::ptr::write(
                out_entities.add(written),
                FfiEntity::from_bits(entity.to_bits()),
            );
        }
        written += 1;
    }
    // SAFETY: `out_read` is non-null, aligned, and writable.
    unsafe {
        *out_read = written;
    }
    FfiResult::OK
}
