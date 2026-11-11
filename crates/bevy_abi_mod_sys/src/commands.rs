//! Opaque deferred command queue handle and command FFI function pointer types.

use core::ffi::c_char;

use crate::primitives::{FfiEntity, FfiResult};

/// Opaque C handle representing a deferred command queue passed to dynamic systems.
///
/// The `_private: [u8; 0]` pattern creates a zero-sized, non-constructible opaque type
/// that prevents foreign code from inspecting or corrupting internal host command queue buffers.
#[repr(C)]
pub struct RawCommandQueue {
    _private: [u8; 0],
}

/// Spawns an empty entity deferred on the Bevy world and returns its allocated [`FfiEntity`] immediately.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution, or null.
pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;

/// Queues deferred despawning of `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution, or null.
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;

/// Queues deferred raw component byte insertion of `component_name` (`len` bytes at `data`) onto `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string.
/// - If `len > 0`, `data` must point to at least `len` readable bytes matching the component layout.
pub type CmdInsertComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
    data: *const u8,
    len: usize,
) -> FfiResult;

/// Queues deferred removal of component `component_name` from `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string.
pub type CmdRemoveComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult;

/// Queues deferred insertion of host-constructed `T::default()` for component `component_name` on `entity`.
///
/// Enables guest mods to attach opaque non-POD host components (such as `String`- or `Vec`-bearing
/// components registered via `register_mod_component_with_default`) without allocator crossover,
/// and subsequently mutate their fields via VTable setters on the next frame.
///
/// Returns [`FfiResult::PERMISSION_DENIED`] if the component was not registered with a `Default` constructor.
///
/// # Safety
/// - `queue` must be a valid pointer to an active [`RawCommandQueue`] during system execution.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string.
pub type CmdInsertDefaultFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult;
