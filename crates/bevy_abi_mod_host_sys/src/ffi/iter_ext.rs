//! Lifecycle `RemovedComponents<T>` batch reader (`iter_read_removed`) entrypoint.

use core::ffi::c_void;

use bevy_abi_mod_sys::{BamIter, FfiEntity, FfiResult};

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
    _term_index: usize,
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
    let Ok(_host_iter) =
        (unsafe { validate_handle_mut::<HostIter<'_>>(iter.cast::<c_void>(), ITER_MAGIC, false) })
    else {
        return if iter.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    FfiResult::UNSUPPORTED
}
