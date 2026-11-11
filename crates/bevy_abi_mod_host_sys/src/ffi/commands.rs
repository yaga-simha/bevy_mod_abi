//! Deferred ECS command bridge and allocator-isolated byte insertion (`MaybeUninit<u8>`).

use crate::registry::ModRegistry;
use crate::sync_ptr::AlignedBytes;
use bevy_abi_mod_sys::{FfiEntity, FfiResult, RawCommandQueue};
use bevy_ecs::entity::{Entity, EntityAllocator};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::{CStr, c_char};
use core::num::NonZeroUsize;

/// Concrete host bridge exposed behind the opaque [`RawCommandQueue`] handle.
pub struct CommandQueueBridge<'a> {
    /// Bevy command queue accumulating deferred mutations.
    pub queue: &'a mut CommandQueue,
    /// Atomic entity allocator for immediate ID generation.
    pub allocator: &'a EntityAllocator,
    /// Active mod registry for component lookup and raw deserialization.
    pub registry: &'a ModRegistry,
}

impl<'a> CommandQueueBridge<'a> {
    /// Creates a new command queue bridge.
    pub fn new(
        queue: &'a mut CommandQueue,
        allocator: &'a EntityAllocator,
        registry: &'a ModRegistry,
    ) -> Self {
        Self {
            queue,
            allocator,
            registry,
        }
    }
}

#[inline]
fn validate_command_entity(entity: FfiEntity) -> Result<Entity, FfiResult> {
    if entity.is_null() {
        return Err(FfiResult::INVALID_ENTITY);
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return Err(FfiResult::INVALID_ENTITY);
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return Err(FfiResult::INVALID_ENTITY);
    }
    Ok(bevy_entity)
}

/// Spawns an empty entity deferred on the Bevy world and returns its 64-bit ID immediately.
///
/// # Safety
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
pub unsafe extern "C" fn host_cmd_spawn_empty(queue: *mut RawCommandQueue) -> FfiEntity {
    if queue.is_null() {
        return FfiEntity::NULL;
    }
    // SAFETY: Caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue.cast::<CommandQueueBridge<'_>>()) };
    let entity = bridge.allocator.alloc();
    bridge.queue.push(move |world: &mut World| {
        let _ = world.spawn_empty_at(entity);
    });
    FfiEntity::from_bits(entity.to_bits())
}

/// Queues deferred despawning of an entity.
///
/// # Safety
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
pub unsafe extern "C" fn host_cmd_despawn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
) -> FfiResult {
    if queue.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    // SAFETY: Caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue.cast::<CommandQueueBridge<'_>>()) };
    bridge.queue.push(move |world: &mut World| {
        if let Ok(e) = world.get_entity_mut(bevy_entity) {
            e.despawn();
        }
    });
    FfiResult::OK
}

/// Queues deferred component insertion onto an entity, copying foreign bytes via `MaybeUninit<u8>`
/// without forming `&[u8]` over uninitialized struct padding bytes.
///
/// # Safety
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null. `component_name` must point
/// to a valid NUL-terminated C string or null. `data` must point to at least `len` readable bytes
/// (padding bytes may be uninitialized) when `len > 0`.
pub unsafe extern "C" fn host_cmd_insert_component(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
    data: *const u8,
    len: usize,
) -> FfiResult {
    if queue.is_null() || component_name.is_null() || (len > 0 && data.is_null()) {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Caller guarantees `component_name` is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    // SAFETY: Caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue.cast::<CommandQueueBridge<'_>>()) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }

    if !comp_meta.is_dynamic && comp_meta.insert_raw_fn.is_none() {
        return FfiResult::PERMISSION_DENIED;
    }

    if comp_meta.size != len {
        return FfiResult::INVALID_ARGUMENT;
    }

    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    let align = comp_meta.align.max(1);
    // SAFETY: `data` is non-null when `len > 0` and points to `len` readable bytes;
    // `AlignedBytes::copy_from_guest` copies as `MaybeUninit<u8>` without forming `&[u8]`.
    let mut buf = match unsafe { AlignedBytes::copy_from_guest(data, len, align) } {
        Ok(b) => b,
        Err(err) => return err,
    };

    if !comp_meta.is_dynamic {
        if let Some(insert_fn) = comp_meta.insert_raw_fn {
            bridge.queue.push(move |world: &mut World| {
                // SAFETY: `buf` holds `comp_meta.size` bytes aligned to `align`; `insert_fn` reads
                // `T` via `ptr::read_unaligned` without forming `&[u8]`.
                unsafe { insert_fn(world, bevy_entity, buf.as_u8_ptr()) };
            });
            return FfiResult::OK;
        }
        return FfiResult::PERMISSION_DENIED;
    }

    let comp_id = comp_meta.component_id;
    bridge.queue.push(move |world: &mut World| {
        if let Ok(mut entity_mut) = world.get_entity_mut(bevy_entity) {
            let non_null = if len > 0 {
                core::ptr::NonNull::new(buf.as_mut_u8_ptr())
            } else {
                NonZeroUsize::new(align).map(core::ptr::NonNull::<u8>::without_provenance)
            };
            if let Some(non_null) = non_null {
                // SAFETY: `non_null` is non-null, aligned to `align`, and points to host-allocated
                // bytes (or a valid aligned ZST address). Ownership of the component value transfers
                // into Bevy's table storage, while `AlignedBytes::drop` frees only the raw backing buffer.
                let owning_ptr = unsafe { OwningPtr::new(non_null) };
                // SAFETY: `comp_id` was registered in this `World` with the exact `(len, align)` layout.
                unsafe {
                    entity_mut.insert_by_id(comp_id, owning_ptr);
                }
            }
        }
        drop(buf);
    });

    FfiResult::OK
}

/// Queues deferred removal of component `component_name` from `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_cmd_remove_component(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult {
    if queue.is_null() || component_name.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Caller guarantees `component_name` is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    // SAFETY: Caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue.cast::<CommandQueueBridge<'_>>()) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }

    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    let comp_id = comp_meta.component_id;
    bridge.queue.push(move |world: &mut World| {
        if let Ok(mut entity_mut) = world.get_entity_mut(bevy_entity) {
            entity_mut.remove_by_id(comp_id);
        }
    });

    FfiResult::OK
}

/// Queues deferred insertion of host-constructed `T::default()` for `component_name` onto `entity`.
///
/// # Safety
/// - `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
/// - `component_name` must point to a valid NUL-terminated UTF-8 C string or null.
pub unsafe extern "C" fn host_cmd_insert_default(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
) -> FfiResult {
    if queue.is_null() || component_name.is_null() {
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: Caller guarantees `component_name` is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    // SAFETY: Caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue.cast::<CommandQueueBridge<'_>>()) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }
    let Some(insert_default_fn) = comp_meta.insert_default_fn else {
        return FfiResult::PERMISSION_DENIED;
    };

    let bevy_entity = match validate_command_entity(entity) {
        Ok(e) => e,
        Err(err) => return err,
    };

    bridge.queue.push(move |world: &mut World| {
        insert_default_fn(world, bevy_entity);
    });

    FfiResult::OK
}
