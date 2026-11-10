#![deny(unsafe_op_in_unsafe_fn)]
#![deny(improper_ctypes_definitions)]

//! # Deferred ECS Commands & Memory Allocator Isolation
//!
//! This module implements [`CommandQueueBridge`] and C-ABI export functions behind
//! the opaque [`RawCommandQueue`] handle.
//!
//! ## Architectural Principles
//! 1. **Why ECS Structural Changes Must Be Deferred**:
//!    - Structural operations (spawning/despawning entities, inserting/removing components)
//!      reallocate and reorganize archetype tables.
//!    - Performing immediate structural operations during table chunk iteration would invalidate
//!      active column pointers and cause memory corruption.
//!    - Bevy defers these commands via [`CommandQueue`], applying them at schedule sync points.
//! 2. **Immediate Entity ID Allocation via [`EntityAllocator`]**:
//!    - When a mod calls `cmd_spawn_empty`, it immediately needs a valid entity ID to insert
//!      components or reference relationships within the same frame.
//!    - `bridge.allocator.alloc()` generates a valid 64-bit [`Entity`] ID synchronously from
//!      Bevy's thread-safe atomic entity allocator, while queueing `world.spawn_empty_at(entity)` deferred.
//! 3. **Strict Allocator Isolation (Rule 2)**:
//!    - Guest mods and the host game may use entirely different memory allocators.
//!    - Memory allocated by guest must never be freed by host, and vice versa.
//!    - In [`host_cmd_insert_component`], the host copies the guest's component byte buffer
//!      into a host-owned `Vec<u8>` via `core::slice::from_raw_parts(data, len).to_vec()`.
//!    - When the command queue is flushed, the host safely inserts the component from host memory.

use crate::registry::ModRegistry;
use abi_contract::{FfiEntity, FfiResult, RawCommandQueue};
use bevy_ecs::entity::{Entity, EntityAllocator};
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::{c_char, CStr};

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

/// C-ABI function pointer type for deferred empty entity creation.
pub type CmdSpawnEmptyFn = unsafe extern "C" fn(queue: *mut RawCommandQueue) -> FfiEntity;

/// C-ABI function pointer type for deferred entity despawning.
pub type CmdDespawnFn =
    unsafe extern "C" fn(queue: *mut RawCommandQueue, entity: FfiEntity) -> FfiResult;

/// C-ABI function pointer type for deferred component insertion.
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
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
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
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
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
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null. `component_name` must point to a valid
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
