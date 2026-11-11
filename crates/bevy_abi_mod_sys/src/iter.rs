//! Opaque archetype chunk iterator handle and column/resource FFI function pointer types.

use core::ffi::c_char;

use crate::primitives::{FfiEntity, FfiResult};

/// Opaque C handle representing an active Archetype Table chunk iterator.
///
/// The `_private: [u8; 0]` pattern creates a zero-sized, non-constructible opaque type
/// that prevents foreign code from inspecting or corrupting internal host iteration buffers.
#[repr(C)]
pub struct RawChunkIter {
    _private: [u8; 0],
}

/// Advances the chunk iterator to the next matching archetype table.
///
/// Returns `0` when exhausted, or the `entity_count` for the active chunk.
///
/// # Safety
/// `iter` must be a valid, non-null pointer to an active [`RawChunkIter`].
pub type IterNextChunkFn = unsafe extern "C" fn(iter: *mut RawChunkIter) -> usize;

/// Obtains a read-only byte pointer to the start of column `index` in the current chunk.
///
/// Returns `null` if `index` is out of bounds or is a filter-only component.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`] with `entity_count > 0`.
/// - `index` must be strictly less than the number of queries declared for this system.
/// - The column must not be filter-only or without.
pub type IterColumnReadonlyFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> *const u8;

/// Obtains a mutable byte pointer to the start of column `index` in the current chunk.
///
/// Marks `ComponentTicks` as changed for all entities in the chunk.
/// Returns `null` if the column was not declared with `ComponentAccessMode::WRITE`.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`] with `entity_count > 0`.
/// - `index` must have been declared with [`crate::ComponentAccessMode::WRITE`].
pub type IterColumnMutableFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize) -> *mut u8;

/// Explicitly marks row `row` in column `index` as changed.
///
/// # Safety
/// - `iter` must be an active [`RawChunkIter`].
/// - `row` must be strictly less than the current chunk entity count.
/// - `index` must have been declared with [`crate::ComponentAccessMode::WRITE`].
pub type IterMarkChangedFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, index: usize, row: usize) -> FfiResult;

/// Obtains a pointer to the contiguous array of `FfiEntity` identifiers for the current chunk.
///
/// # Safety
/// `iter` must be an active [`RawChunkIter`]; returns a pointer to `[FfiEntity; entity_count]`.
pub type IterEntitiesFn = unsafe extern "C" fn(iter: *const RawChunkIter) -> *const FfiEntity;

/// Returns the byte stride (`size_of::<T>()`) of column `index` for array indexing.
///
/// # Safety
/// `iter` must be a valid pointer to a [`RawChunkIter`].
pub type IterColumnStrideFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, index: usize) -> usize;

/// Resets a `RawChunkIter` cursor back to its initial matched archetype table so inner queries
/// can be re-traversed inside nested query loops with zero heap allocations.
///
/// # Safety
/// `iter` must be a valid pointer to a [`RawChunkIter`].
pub type IterResetFn = unsafe extern "C" fn(iter: *mut RawChunkIter) -> FfiResult;

/// Obtains a read-only byte pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Returns `null` if `iter` or `resource_name` is null, if the resource was not declared with
/// [`crate::ComponentAccessMode::RESOURCE_READ`] or [`crate::ComponentAccessMode::RESOURCE_WRITE`] on this system,
/// or if the resource is not currently inserted in the `World`.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`RawChunkIter`] passed to the system callback.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string.
pub type IterResourceReadonlyFn =
    unsafe extern "C" fn(iter: *const RawChunkIter, resource_name: *const c_char) -> *const u8;

/// Obtains an exclusive mutable byte pointer to singleton Bevy `Resource` `resource_name` during system execution.
///
/// Marks the resource's Bevy change tick as changed (`Res<R>::is_changed()` becomes `true`).
/// Returns `null` if `iter` or `resource_name` is null, if the resource was not declared strictly with
/// [`crate::ComponentAccessMode::RESOURCE_WRITE`] on this system, or if the resource is not present in the `World`.
///
/// # Safety
/// - `iter` must be a valid pointer to an active [`RawChunkIter`] passed to the system callback.
/// - `resource_name` must point to a valid NUL-terminated UTF-8 C string.
/// - Caller must not create aliasing references to the same resource simultaneously.
pub type IterResourceMutableFn =
    unsafe extern "C" fn(iter: *mut RawChunkIter, resource_name: *const c_char) -> *mut u8;
