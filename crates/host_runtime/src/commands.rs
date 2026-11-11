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
//! 3. **Strict Allocator & Layout Isolation (Rule 2 + Pod Safety)**:
//!    - Guest mods and the host game may use entirely different memory allocators.
//!    - Memory allocated by guest must never be freed by host, and vice versa.
//!    - Static host components that manage heap allocations (e.g. `String`, `Vec`) or non-Pod
//!      invariants forbid raw byte insertion (`FfiResult::PERMISSION_DENIED`) so guest bytes
//!      can never forge host heap pointers or cause double-frees.
//!    - For dynamic components, [`host_cmd_insert_component`] copies foreign bytes into an
//!      aligned host-owned buffer (or constructs a properly aligned dangling pointer for ZSTs)
//!      before transferring ownership into Bevy via [`OwningPtr`].

use crate::registry::ModRegistry;
pub use abi_contract::{CmdInsertDefaultFn, CmdRemoveComponentFn};
use abi_contract::{FfiEntity, FfiResult, RawCommandQueue};
use bevy_ecs::entity::{Entity, EntityAllocator};
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::{CommandQueue, World};
use core::ffi::{c_char, CStr};
use core::num::NonZeroUsize;
use std::alloc::Layout;

/// Host-allocated temporary buffer aligned to the component's required memory alignment.
/// Automatically deallocates the aligned host memory when dropped.
struct AlignedBuffer {
    ptr: usize,
    layout: Layout,
}

// SAFETY: Host memory buffer ownership transferred into deferred command closure.
unsafe impl Send for AlignedBuffer {}
unsafe impl Sync for AlignedBuffer {}

impl Drop for AlignedBuffer {
    fn drop(&mut self) {
        if self.layout.size() > 0 && self.ptr != 0 {
            // SAFETY: Memory was allocated with this exact layout via std::alloc::alloc.
            unsafe {
                std::alloc::dealloc(self.ptr as *mut u8, self.layout);
            }
        }
    }
}

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
/// Returns [`FfiEntity::NULL`] (`0`) if `queue` is null.
///
/// # Safety
/// `queue` must be a valid pointer to a [`CommandQueueBridge`] or null.
pub unsafe extern "C" fn host_cmd_spawn_empty(queue: *mut RawCommandQueue) -> FfiEntity {
    if queue.is_null() {
        return FfiEntity::NULL;
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
/// Rejects [`FfiEntity::NULL`] (`0`), invalid bit patterns, and `Entity::PLACEHOLDER`.
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
    if entity.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INTERNAL_ERROR;
    }

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

    // SAFETY: caller passes a valid pointer to CommandQueueBridge cast to RawCommandQueue.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }

    // Static components that are non-Pod / heap-owning (`insert_raw_fn == None`) must NEVER
    // accept raw foreign byte buffers or fall through to `insert_by_id` (D3 soundness guard).
    if !comp_meta.is_dynamic && comp_meta.insert_raw_fn.is_none() {
        return FfiResult::PERMISSION_DENIED;
    }

    if comp_meta.size != len {
        return FfiResult::INTERNAL_ERROR;
    }

    // Reject FfiEntity::NULL (0), invalid entity bits, and Bevy's Entity::PLACEHOLDER (D9 guard).
    if entity.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INTERNAL_ERROR;
    }

    // Static component fast-path using monomorphized insert_raw_fn
    if !comp_meta.is_dynamic {
        if let Some(insert_fn) = comp_meta.insert_raw_fn {
            let host_bytes = if len > 0 {
                // SAFETY: data is non-null and points to at least len readable bytes.
                unsafe { core::slice::from_raw_parts(data, len) }.to_vec()
            } else {
                Vec::new()
            };
            bridge.queue.push(move |world: &mut World| {
                // SAFETY: host_bytes matches comp_meta.size and insert_fn safely unmarshals it.
                unsafe { insert_fn(world, bevy_entity, &host_bytes) };
            });
            return FfiResult::OK;
        }
        return FfiResult::PERMISSION_DENIED;
    }

    // Dynamic component deferred insertion via insert_by_id and host-aligned buffer
    let comp_id = comp_meta.component_id;
    let align = comp_meta.align.max(1);
    let buf = if len > 0 {
        let Ok(layout) = Layout::from_size_align(comp_meta.size, align) else {
            return FfiResult::INTERNAL_ERROR;
        };
        // SAFETY: Allocating host memory matching component's layout (Rule 2: strict allocator isolation).
        let host_ptr = unsafe { std::alloc::alloc(layout) };
        if host_ptr.is_null() {
            return FfiResult::INTERNAL_ERROR;
        }
        // SAFETY: Copying foreign bytes into host-allocated memory.
        unsafe {
            core::ptr::copy_nonoverlapping(data, host_ptr, len);
        }
        AlignedBuffer {
            ptr: host_ptr as usize,
            layout,
        }
    } else {
        let Ok(layout) = Layout::from_size_align(0, align) else {
            return FfiResult::INTERNAL_ERROR;
        };
        AlignedBuffer { ptr: align, layout }
    };

    bridge.queue.push(move |world: &mut World| {
        let _keep_alive = &buf;
        if let Ok(mut entity_mut) = world.get_entity_mut(bevy_entity) {
            let non_null = if buf.layout.size() > 0 {
                core::ptr::NonNull::new(buf.ptr as *mut u8)
            } else {
                // D10 fix: construct a non-null dangling pointer aligned to `buf.layout.align()`
                // rather than `NonNull::<u8>::dangling()` (which only guarantees align 1).
                NonZeroUsize::new(buf.layout.align())
                    .map(core::ptr::NonNull::<u8>::without_provenance)
            };
            if let Some(non_null) = non_null {
                // SAFETY: `non_null` is non-null, properly aligned for `comp_id`'s layout, and
                // points to initialized host memory (or a valid aligned ZST address). Ownership of
                // the component bytes transfers to Bevy; `AlignedBuffer::drop` only frees the raw
                // backing allocation without running any Rust destructor.
                let owning_ptr = unsafe { OwningPtr::new(non_null) };
                // SAFETY: `comp_id` was registered in this World with the exact layout of `buf`.
                unsafe {
                    entity_mut.insert_by_id(comp_id, owning_ptr);
                }
            }
        }
        // buf is explicitly owned by the closure and dropped here after insertion.
        drop(buf);
    });

    FfiResult::OK
}

/// Queues deferred removal of component `component_name` from `entity`.
///
/// Works for both statically registered host components and dynamically registered guest components.
/// Rejects resources, unregistered components, [`FfiEntity::NULL`], invalid bit patterns, and `Entity::PLACEHOLDER`.
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

    // SAFETY: caller guarantees `component_name` is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    // SAFETY: caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }

    if entity.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INTERNAL_ERROR;
    }

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
/// Enables guest mods to spawn or attach opaque non-POD host components (such as `String`- or
/// `Vec`-bearing components registered via `register_mod_component_with_default`) without raw
/// byte deserialization or allocator crossover, and subsequently mutate fields via VTable setters.
///
/// Returns [`FfiResult::PERMISSION_DENIED`] if the component was not registered with a `Default` constructor.
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

    // SAFETY: caller guarantees `component_name` is a valid NUL-terminated C string.
    let Ok(name) = (unsafe { CStr::from_ptr(component_name) }).to_str() else {
        return FfiResult::INVALID_UTF8;
    };

    // SAFETY: caller passes a valid pointer to `CommandQueueBridge` cast to `RawCommandQueue`.
    let bridge = unsafe { &mut *(queue as *mut CommandQueueBridge<'_>) };
    let Some(comp_meta) = bridge.registry.get_by_name(name) else {
        return FfiResult::INVALID_COMPONENT;
    };
    if comp_meta.is_resource {
        return FfiResult::INVALID_COMPONENT;
    }
    let Some(insert_default_fn) = comp_meta.insert_default_fn else {
        return FfiResult::PERMISSION_DENIED;
    };

    if entity.is_null() {
        return FfiResult::INTERNAL_ERROR;
    }
    let Some(bevy_entity) = Entity::try_from_bits(entity.to_bits()) else {
        return FfiResult::INTERNAL_ERROR;
    };
    if bevy_entity == Entity::PLACEHOLDER {
        return FfiResult::INTERNAL_ERROR;
    }

    bridge.queue.push(move |world: &mut World| {
        insert_default_fn(world, bevy_entity);
    });

    FfiResult::OK
}
