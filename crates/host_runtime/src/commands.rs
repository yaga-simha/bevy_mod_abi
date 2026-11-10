#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

use crate::registry::ModRegistry;
use abi_contract::{FfiEntity, FfiResult, RawCommandQueue};
use bevy_ecs::entity::{Entity, EntityAllocator};
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::{c_char, CStr};

/// Concrete host bridge exposed behind the opaque `RawCommandQueue` handle.
pub struct CommandQueueBridge<'a> {
    pub queue: &'a mut CommandQueue,
    pub allocator: &'a EntityAllocator,
    pub registry: &'a ModRegistry,
}

impl<'a> CommandQueueBridge<'a> {
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

pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;
pub type CmdInsertComponentFn = unsafe extern "C" fn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
    component_name: *const c_char,
    data: *const u8,
    len: usize,
) -> FfiResult;

/// Spawns an empty entity deferred on the Bevy world and returns its 64-bit ID immediately.
///
/// # Safety
/// `queue` must be a valid pointer to a `CommandQueueBridge` or null.
pub unsafe extern "C" fn host_cmd_spawn_empty(queue: *mut RawCommandQueue) -> FfiEntity {
    if queue.is_null() {
        return FfiEntity(0);
    }
    // SAFETY: caller passes a valid pointer to CommandQueueBridge cast to RawCommandQueue.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    let entity = bridge.allocator.alloc();
    bridge.queue.push(move |world: &mut World| {
        let _ = world.spawn_empty_at(entity);
    });
    FfiEntity::from_bits(entity.to_bits())
}

/// Queues deferred despawning of an entity.
///
/// # Safety
/// `queue` must be a valid pointer to a `CommandQueueBridge` or null.
pub unsafe extern "C" fn host_cmd_despawn(
    queue: *mut RawCommandQueue,
    entity: FfiEntity,
) -> FfiResult {
    if queue.is_null() {
        return FfiResult::NULL_POINTER;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };

    // SAFETY: caller passes a valid pointer to CommandQueueBridge cast to RawCommandQueue.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    bridge.queue.push(move |world: &mut World| {
        if let Ok(e) = world.get_entity_mut(bevy_entity) {
            e.despawn();
        }
    });
    FfiResult::OK
}

/// Queues deferred component insertion onto an entity, copying foreign memory to ensure host allocator isolation.
///
/// # Safety
/// `queue` must be a valid pointer to a `CommandQueueBridge` or null. `component_name` must point to a valid
/// NUL-terminated C string. `data` must point to at least `len` readable bytes if `len > 0`.
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

    // SAFETY: caller guarantees component_name is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };

    // SAFETY: caller passes a valid pointer to CommandQueueBridge cast to RawCommandQueue.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };

    if comp_meta.size != len {
        return FfiResult::INTERNAL_ERROR;
    }

    // Strict Allocator Isolation: allocate host-owned Vec<u8> and copy bytes from guest memory.
    let host_bytes = if len > 0 {
        // SAFETY: data is non-null and points to at least len readable bytes.
        unsafe { core::slice::from_raw_parts(data, len) }.to_vec()
    } else {
        Vec::new()
    };

    let insert_fn = comp_meta.insert_raw_fn;
    bridge.queue.push(move |world: &mut World| {
        // SAFETY: host_bytes matches comp_meta.size and insert_fn safely unmarshals it.
        unsafe { insert_fn(world, bevy_entity, &host_bytes) };
    });

    FfiResult::OK
}
