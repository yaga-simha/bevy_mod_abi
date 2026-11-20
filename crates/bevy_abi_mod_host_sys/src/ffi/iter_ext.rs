//! Extended iterator entrypoints: lifecycle `RemovedComponents<T>` batch reading
//! (`iter_read_removed`) and zero-arena read-only parallel chunk dispatch
//! (`par_for_each_chunk_readonly`).

use core::ffi::c_void;

use bevy_abi_mod_sys::{AccessMode, BamChunkTaskFn, BamIter, FfiEntity, FfiResult};
use bevy_ecs::entity::Entity;
use bevy_ecs::lifecycle::RemovedComponentEntity;
use bevy_ecs::message::MessageCursor;

use crate::{
    ffi::iter::par_for_each_chunk_impl,
    handle_header::{ffi_try, init_out_ptr, validate_iter_mut},
};

/// Reads up to `max_count` removed-component [`FfiEntity`] values for `term_index` (`REMOVED_READ`).
///
/// # Safety
/// - `iter` must point to an active [`crate::ffi::iter::HostIter`] cast to [`BamIter`], or null.
/// - `out_read` must point to writable `usize` storage, or null.
/// - When `max_count > 0`, `out_entities` must point to `max_count` writable [`FfiEntity`] slots.
pub unsafe extern "C" fn host_iter_read_removed(
    iter: *mut BamIter,
    term_index: usize,
    out_entities: *mut FfiEntity,
    max_count: usize,
    out_read: *mut usize,
) -> FfiResult {
    if max_count > 0 && out_entities.is_null() {
        return FfiResult::NULL_POINTER;
    }
    if max_count > 0 && !out_entities.is_aligned() {
        return FfiResult::INVALID_ARGUMENT;
    }
    // SAFETY: `out_read` is checked for null and alignment before writing `0`.
    ffi_try!(unsafe { init_out_ptr(out_read, 0) });
    // SAFETY: Validates `iter` against `ITER_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_iter = ffi_try!(unsafe { validate_iter_mut(iter) });
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

/// Core implementation backing ABI `0.3.0` [`crate::host_par_for_each_chunk_readonly`].
///
/// Enforces that every declared term in `iter`'s query plan is strictly read-only (rejecting any
/// `WRITE`, `OPTIONAL_WRITE`, `RESOURCE_WRITE`, or `MESSAGE_WRITE` term with
/// [`FfiResult::PERMISSION_DENIED`]), then dispatches disjoint table/archetype sub-ranges across
/// Bevy's [`bevy_tasks::ComputeTaskPool`] with zero command-arena allocations and zero post-join
/// command-buffer merges.
///
/// # Safety
/// - `iter` must point to an active [`HostIter`] cast to [`BamIter`], or null.
/// - `chunk_fn` and `user_data` must be thread-safe (`Sync`) across concurrent worker invocations
///   and must never unwind across `extern "C"`.
pub(crate) unsafe fn par_for_each_chunk_readonly_impl(
    iter: *mut BamIter,
    chunk_fn: Option<BamChunkTaskFn>,
    user_data: *mut c_void,
) -> FfiResult {
    // SAFETY: Forwarding under identical FFI safety contract with `require_readonly = true`.
    unsafe { par_for_each_chunk_impl(iter, chunk_fn, user_data, true) }
}

#[cfg(test)]
#[path = "../../tests/unit/iter_ext_tests.rs"]
mod tests;
