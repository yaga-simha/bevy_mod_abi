//! Opaque deferred command buffer handle and handle-based/batched command FFI function types.

use core::marker::{PhantomData, PhantomPinned};

use crate::primitives::{BamComponent, FfiEntity, FfiResult};

/// Opaque C handle representing the per-system deferred command buffer (`typedef struct BamCommands BamCommands;`).
#[repr(C)]
pub struct BamCommands {
    _private: [u8; 0],
    _marker: PhantomData<(*mut u8, PhantomPinned)>,
}

/// Reserves a fresh empty entity atomically and writes its [`FfiEntity`] identifier into `*out_entity`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `out_entity` must point to writable [`FfiEntity`] storage.
pub type BamCmdSpawnEmptyFn =
    unsafe extern "C" fn(commands: *mut BamCommands, out_entity: *mut FfiEntity) -> FfiResult;

/// Reserves a fresh entity and queues a batched multi-component spawn (`components[0..count]` with
/// byte payloads `data_ptrs[0..count]`), placing all components in a single archetype transition
/// during `apply_deferred`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - `out_entity` must point to writable [`FfiEntity`] storage.
/// - If `count > 0`, `components` must point to `count` readable [`BamComponent`] handles, and
///   `data_ptrs` must point to `count` pointers where each `data_ptrs[i]` is readable for the
///   registered byte size of `components[i]` (when `size > 0`).
pub type BamCmdSpawnFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    components: *const BamComponent,
    data_ptrs: *const *const u8,
    count: usize,
    out_entity: *mut FfiEntity,
) -> FfiResult;

/// Queues deferred despawning of `entity`.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdDespawnFn =
    unsafe extern "C" fn(commands: *mut BamCommands, entity: FfiEntity) -> FfiResult;

/// Queues deferred raw byte insertion of pre-resolved `component` (`data_len` bytes at `data_ptr`)
/// onto `entity`.
///
/// # Safety
/// - `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
/// - If `data_len > 0`, `data_ptr` must point to at least `data_len` readable bytes matching the
///   registered component layout (including any struct padding bytes).
pub type BamCmdInsertFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
    data_ptr: *const u8,
    data_len: usize,
) -> FfiResult;

/// Queues deferred insertion of host-constructed `T::default()` for pre-resolved `component` on `entity`.
///
/// Returns [`FfiResult::PERMISSION_DENIED`] if `component` was not registered with a `Default` constructor.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdInsertDefaultFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult;

/// Queues deferred removal of pre-resolved `component` from `entity`.
///
/// # Safety
/// `commands` must point to an active [`BamCommands`] belonging to the running system invocation.
pub type BamCmdRemoveFn = unsafe extern "C" fn(
    commands: *mut BamCommands,
    entity: FfiEntity,
    component: BamComponent,
) -> FfiResult;
