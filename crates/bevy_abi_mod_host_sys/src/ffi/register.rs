//! Transactional `LoadContext` (`BamLoadCtx`), `host_register_component`, `host_resolve_component`,
//! and unified `host_register_system`.

use crate::drop_trampoline::{
    StagedDropBinding, StagedHookBinding, WorldDropGuard, increment_drop_slot_live_values,
    on_add_hook_for_slot, on_insert_hook_for_slot, on_remove_hook_for_slot,
    prune_drop_trampolines_for_guard,
};
use crate::handle_header::{
    HOST_CTX_MAGIC, HandleHeader, HostContext, LOAD_CTX_MAGIC, validate_handle_mut,
    validate_handle_ref,
};
use crate::loader::ModId;
use crate::registry::{
    ExtractEventTargetFn, ModRegistry, RegistrySnapshot, component_flags_to_storage_flags,
    extract_owned_dynamic_fields,
};
use crate::schedule::ModNamedSystemSet;
use crate::sync_ptr::{GuestFnPtr, GuestUserData};
use crate::system::access_builder::validate_system_queries_against_snapshot;
use crate::system::{BamQuerySpecOwned, GuestSystem};
use bevy_abi_mod_sys::{
    BAM_REQUIRED_FLAG_CUSTOM_BYTES, BAM_REQUIRED_FLAG_INIT_FN, BAM_REQUIRED_KNOWN_FLAGS,
    BamComponent, BamEventKind, BamHostCtx, BamLoadCtx, BamObserverDesc, BamObserverFn,
    BamObserverMutFn, BamQueryDesc, BamRequiredComponent, BamSystem, BamSystemDesc, BamTermDesc,
    ComponentCloneFn, ComponentDesc, ComponentHookFn, ComponentUuid, FfiEntity, FfiResult,
    read_descriptor,
};
use bevy_ecs::component::{ComponentCloneBehavior, ComponentDescriptor, ComponentId, StorageType};
use bevy_ecs::entity::{ComponentCloneCtx, Entity, SourceComponent};
use bevy_ecs::event::EventKey;
use bevy_ecs::lifecycle::HookContext;
use bevy_ecs::prelude::Resource;
use bevy_ecs::ptr::OwningPtr;
use bevy_ecs::world::{DeferredWorld, World};
use bevy_platform::collections::{HashMap, HashSet};
use core::alloc::Layout;
use core::ffi::{CStr, c_char, c_void};
use core::sync::atomic::{AtomicBool, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

/// Dynamic Bevy ECS observer registration captured during `bevy_abi_mod_init` or runtime `cmd_observe`.
#[derive(Clone)]
pub struct GuestObserverBinding {
    /// Human-readable observer name for diagnostics.
    pub name: Arc<str>,
    /// Pre-resolved 1-based [`BamComponent`] handle of the observed event (`INVALID` for lifecycle).
    pub event: BamComponent,
    /// Observed event kind (`CUSTOM`, `ADD`, `INSERT`, `DISCARD`, `REMOVE`, `DESPAWN`, or `READY`).
    pub event_kind: BamEventKind,
    /// Underlying Bevy [`ComponentId`] corresponding to `event`, if any.
    pub event_id: Option<ComponentId>,
    /// Bevy [`EventKey`] dispatched by `world.trigger*` / `world.trigger_dynamic*`.
    pub event_key: EventKey,
    /// Payload size (`size_of::<E>()`).
    pub event_size: usize,
    /// Whether `guest_observer_runner` synthesizes an aligned [`FfiEntity`] payload on the stack
    /// rather than passing raw `&E` bytes.
    pub synthesizes_ffi_entity_payload: bool,
    /// Optional watched target entity (`None` for global observers).
    pub target_entity: Option<Entity>,
    /// Watched component IDs for lifecycle / component-scoped observers.
    pub watched_components: Box<[ComponentId]>,
    /// Extractor returning the target entity from typed host `EntityEvent` payloads.
    pub extract_target_fn: Option<ExtractEventTargetFn>,
    /// Optional read-only foreign C-ABI callback (`BamObserverFn`).
    pub callback: Option<GuestFnPtr<BamObserverFn>>,
    /// Optional mutable foreign C-ABI callback (`BamObserverMutFn`).
    pub mut_callback: Option<GuestFnPtr<BamObserverMutFn>>,
    /// Whether propagation control is enabled (`BAM_OBSERVER_FLAG_PROPAGATE`).
    pub is_propagating: bool,
    /// Optional relationship component ID for propagation (`None` defaults to `ChildOf`).
    pub propagate_relationship: Option<ComponentId>,
    /// Opaque guest state pointer forwarded to `callback` / `mut_callback`.
    pub user_data: GuestUserData,
    /// Shared module liveness flag disarmed before `dlclose`.
    pub is_active: Arc<AtomicBool>,
    /// CoW registry handle for snapshot lookup inside commands recorded by the observer.
    pub registry: ModRegistry,
    /// Owning [`ModId`].
    pub mod_id: Option<ModId>,
}

// ============================================================================
// Per-World Dynamic Component Lifecycle Hooks & Transitive Required Components
// ============================================================================

#[derive(Clone)]
struct DynamicHookBinding {
    on_add: Option<GuestFnPtr<ComponentHookFn>>,
    on_insert: Option<GuestFnPtr<ComponentHookFn>>,
    on_discard: Option<GuestFnPtr<ComponentHookFn>>,
    on_remove: Option<GuestFnPtr<ComponentHookFn>>,
    on_despawn: Option<GuestFnPtr<ComponentHookFn>>,
    user_data: GuestUserData,
    is_active: Arc<AtomicBool>,
    required_components: Arc<[BamComponent]>,
    registry: ModRegistry,
}

#[derive(Clone)]
struct CloneBindingEntry {
    clone_fn: GuestFnPtr<ComponentCloneFn>,
    user_data: GuestUserData,
    layout: Layout,
    is_active: Arc<AtomicBool>,
}

#[derive(Resource, Default)]
pub(crate) struct DynamicComponentHookMap {
    by_id: HashMap<ComponentId, Vec<DynamicHookBinding>>,
    drop_slot_by_id: HashMap<ComponentId, usize>,
    clone_by_id: HashMap<ComponentId, Vec<CloneBindingEntry>>,
    rel_to_target: HashMap<ComponentId, ComponentId>,
    target_to_rel: HashMap<ComponentId, ComponentId>,
    rel_targets: HashMap<(ComponentId, Entity), Vec<FfiEntity>>,
}

/// Ensures the per-`World` dynamic component hook/clone dispatch table is pre-initialized so
/// registering dynamic components never lazily allocates an `IsResource` entity mid-session.
#[inline]
pub fn ensure_dynamic_hook_map_in_world(world: &mut World) {
    if !world.contains_resource::<DynamicComponentHookMap>() {
        world.insert_resource(DynamicComponentHookMap::default());
    }
}

impl DynamicComponentHookMap {
    #[inline]
    pub(crate) fn is_relationship_target(&self, target_comp_id: ComponentId) -> bool {
        self.target_to_rel.contains_key(&target_comp_id)
    }

    #[inline]
    pub(crate) fn relationship_targets_for(
        &self,
        target_comp_id: ComponentId,
        entity: Entity,
    ) -> &[FfiEntity] {
        self.rel_targets
            .get(&(target_comp_id, entity))
            .map_or(&[], Vec::as_slice)
    }

    pub(crate) fn active_clone_binding(
        &self,
        comp_id: ComponentId,
        expected_layout: Layout,
    ) -> Option<(ComponentCloneFn, *mut c_void)> {
        self.clone_by_id
            .get(&comp_id)
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.is_active.load(Ordering::Acquire))
            })
            .filter(|entry| entry.layout == expected_layout)
            .map(|entry| (entry.clone_fn.0, entry.user_data.as_ptr()))
    }

    fn insert_or_update(&mut self, component_id: ComponentId, binding: DynamicHookBinding) {
        let entries = self.by_id.entry(component_id).or_default();
        entries.retain(|e| e.is_active.load(Ordering::Acquire));
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| Arc::ptr_eq(&e.is_active, &binding.is_active))
        {
            *existing = binding;
        } else {
            entries.push(binding);
        }
    }

    fn set_drop_slot(&mut self, component_id: ComponentId, slot: usize) {
        self.drop_slot_by_id.insert(component_id, slot);
    }

    fn insert_or_update_clone(&mut self, component_id: ComponentId, entry: CloneBindingEntry) {
        let entries = self.clone_by_id.entry(component_id).or_default();
        entries.retain(|e| e.is_active.load(Ordering::Acquire));
        if let Some(existing) = entries
            .iter_mut()
            .find(|e| Arc::ptr_eq(&e.is_active, &entry.is_active))
        {
            *existing = entry;
        } else {
            entries.push(entry);
        }
    }

    fn bind_relationship_pair(&mut self, rel_cid: ComponentId, tgt_cid: ComponentId) {
        self.rel_to_target.insert(rel_cid, tgt_cid);
        self.target_to_rel.insert(tgt_cid, rel_cid);
    }

    fn rollback_for_guard(
        &mut self,
        is_active: &Arc<AtomicBool>,
        fresh_comp_ids: &[ComponentId],
        fresh_rel_bindings: &[(ComponentId, ComponentId)],
    ) {
        self.by_id.retain(|_, entries| {
            entries.retain(|e| {
                !Arc::ptr_eq(&e.is_active, is_active) && e.is_active.load(Ordering::Acquire)
            });
            !entries.is_empty()
        });
        self.clone_by_id.retain(|_, entries| {
            entries.retain(|e| {
                !Arc::ptr_eq(&e.is_active, is_active) && e.is_active.load(Ordering::Acquire)
            });
            !entries.is_empty()
        });
        for comp_id in fresh_comp_ids {
            self.drop_slot_by_id.remove(comp_id);
        }
        for &(rel_cid, tgt_cid) in fresh_rel_bindings {
            self.rel_to_target.remove(&rel_cid);
            self.target_to_rel.remove(&tgt_cid);
        }
    }
}

struct OwnedZeroedBuffer {
    ptr: *mut u8,
    layout: Layout,
}

impl OwnedZeroedBuffer {
    fn new(layout: Layout) -> Self {
        if layout.size() == 0 {
            return Self {
                ptr: core::ptr::NonNull::<u8>::dangling().as_ptr(),
                layout,
            };
        }
        // SAFETY: `layout.size() > 0` and `layout.align()` is a valid power of two.
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        if ptr.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        Self { ptr, layout }
    }
}

impl Drop for OwnedZeroedBuffer {
    fn drop(&mut self) {
        if self.layout.size() > 0 {
            // SAFETY: `self.ptr` was allocated with `std::alloc::alloc_zeroed(self.layout)`.
            unsafe {
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }
}

fn insert_required_components_recursive(
    world: &mut World,
    entity: Entity,
    root_reqs: &[BamComponent],
    registry: &ModRegistry,
) {
    if root_reqs.is_empty() || !world.entities().contains(entity) {
        return;
    }
    let snap = registry.latest_snapshot_arc();
    let mut visited = HashSet::<BamComponent>::new();
    let mut queue = VecDeque::<BamComponent>::new();
    for &req in root_reqs {
        if visited.insert(req) {
            queue.push_back(req);
        }
    }

    while let Some(req_handle) = queue.pop_front() {
        let Some(req_meta) = snap.get_by_handle(req_handle) else {
            continue;
        };
        for &transitive in snap.required_components(req_handle) {
            if visited.insert(transitive) {
                queue.push_back(transitive);
            }
        }

        let Ok(ent_ref) = world.get_entity(entity) else {
            return;
        };
        if ent_ref.contains_id(req_meta.component_id) {
            continue;
        }

        if let Some(insert_default) = req_meta.insert_default_fn {
            insert_default(world, entity);
        } else if req_meta.is_abi_pod && req_meta.drop_fn.is_none() && req_meta.drop_slot.is_none()
        {
            let Ok(layout) = Layout::from_size_align(req_meta.size, req_meta.align.max(1)) else {
                continue;
            };
            let zeroed = OwnedZeroedBuffer::new(layout);
            if !req_meta.is_dynamic {
                if let Some(insert_raw) = req_meta.insert_raw_fn {
                    // SAFETY: `zeroed.ptr` holds `req_meta.size` zeroed bytes aligned to
                    // `req_meta.align`, and `req_meta.is_abi_pod` guarantees zero-init validity.
                    unsafe {
                        insert_raw(world, entity, zeroed.ptr.cast_const());
                    }
                }
            } else if let Ok(mut e_mut) = world.get_entity_mut(entity) {
                let nn = if layout.size() == 0 {
                    core::ptr::NonNull::<u8>::dangling()
                } else {
                    // SAFETY: `OwnedZeroedBuffer::new` aborts on null when `size > 0`.
                    unsafe { core::ptr::NonNull::new_unchecked(zeroed.ptr) }
                };
                // SAFETY: `nn` points to `req_meta.size` initialized zeroed bytes matching
                // `req_meta.component_id`'s layout; `insert_by_id` copies out without freeing `nn`.
                let owning = unsafe { OwningPtr::new(nn) };
                // SAFETY: `req_meta.component_id` was registered in `world` with `(size, align)`.
                unsafe {
                    e_mut.insert_by_id(req_meta.component_id, owning);
                }
            }
        }
    }
}

fn active_hook_binding<'a>(
    world: &'a DeferredWorld<'_>,
    component_id: ComponentId,
) -> Option<&'a DynamicHookBinding> {
    let map = world.get_resource::<DynamicComponentHookMap>()?;
    let entries = map.by_id.get(&component_id)?;
    entries
        .iter()
        .rev()
        .find(|e| e.is_active.load(Ordering::Acquire))
}

fn invoke_dynamic_hook(
    world: &DeferredWorld<'_>,
    ctx: HookContext,
    select: fn(&DynamicHookBinding) -> Option<GuestFnPtr<ComponentHookFn>>,
) {
    let Some(entry) = active_hook_binding(world, ctx.component_id) else {
        return;
    };
    let Some(GuestFnPtr(cb)) = select(entry) else {
        return;
    };
    let Ok(comp_ptr) = world
        .entity(ctx.entity)
        .get_by_id(ctx.component_id)
        .map(|p| p.as_ptr().cast::<c_void>())
    else {
        return;
    };
    let ffi_entity = FfiEntity::from_bits(ctx.entity.to_bits());
    // SAFETY: `entry.is_active` is verified true (`Ordering::Acquire`), `cb` remains mapped in
    // memory for this synchronous hook call, and `comp_ptr` points to the live component value.
    unsafe {
        cb(ffi_entity, comp_ptr, entry.user_data.as_ptr());
    }
}

fn dynamic_on_add_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    let req_info = active_hook_binding(&world, ctx.component_id).and_then(|entry| {
        if entry.required_components.is_empty() {
            None
        } else {
            Some((
                Arc::clone(&entry.required_components),
                entry.registry.clone(),
            ))
        }
    });
    if let Some((reqs, registry)) = req_info {
        world.commands().queue(move |w: &mut World| {
            insert_required_components_recursive(w, ctx.entity, &reqs, &registry);
        });
    }
    invoke_dynamic_hook(&world, ctx, |e| e.on_add);
}

fn dynamic_rel_on_insert(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let Some(tgt_cid) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| map.rel_to_target.get(&ctx.component_id).copied())
    else {
        return;
    };
    let target_ffi = {
        let Ok(ptr) = world.entity(ctx.entity).get_by_id(ctx.component_id) else {
            return;
        };
        // SAFETY: Dynamic relationship source components are validated as `(size = 8, align = 8)`
        // storing an `FfiEntity` target handle at offset 0.
        let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
        FfiEntity::from_bits(bits)
    };
    let valid_target = (!target_ffi.is_null())
        .then(|| Entity::try_from_bits(target_ffi.to_bits()))
        .flatten()
        .filter(|&t| t != Entity::PLACEHOLDER && t != ctx.entity && world.entities().contains(t));
    let Some(target_entity) = valid_target else {
        let source_entity = ctx.entity;
        let rel_cid = ctx.component_id;
        world.commands().queue(move |w: &mut World| {
            if let Ok(mut e) = w.get_entity_mut(source_entity) {
                e.remove_by_id(rel_cid);
            }
        });
        return;
    };

    let source_ffi = FfiEntity::from_bits(ctx.entity.to_bits());
    if let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() {
        let vec = map.rel_targets.entry((tgt_cid, target_entity)).or_default();
        if !vec.contains(&source_ffi) {
            vec.push(source_ffi);
        }
    }
    if world
        .get_entity(target_entity)
        .is_ok_and(|ent| !ent.contains_id(tgt_cid))
    {
        world.commands().queue(move |w: &mut World| {
            let should_insert = w
                .get_resource::<DynamicComponentHookMap>()
                .is_some_and(|m| {
                    !m.relationship_targets_for(tgt_cid, target_entity)
                        .is_empty()
                });
            if should_insert
                && let Ok(mut target_ent) = w.get_entity_mut(target_entity)
                && !target_ent.contains_id(tgt_cid)
            {
                // SAFETY: `tgt_cid` was registered with zero-sized `(0, 1)` layout.
                let owning = unsafe { OwningPtr::new(core::ptr::NonNull::<u8>::dangling()) };
                // SAFETY: `tgt_cid` is a valid zero-sized component in `w`.
                unsafe {
                    target_ent.insert_by_id(tgt_cid, owning);
                }
            }
        });
    }
}

fn dynamic_rel_on_discard(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let Some(tgt_cid) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| map.rel_to_target.get(&ctx.component_id).copied())
    else {
        return;
    };
    let old_target_ffi = {
        let Ok(ptr) = world.entity(ctx.entity).get_by_id(ctx.component_id) else {
            return;
        };
        // SAFETY: Dynamic relationship source components are validated as `(size = 8, align = 8)`
        // storing an `FfiEntity` target handle at offset 0.
        let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
        FfiEntity::from_bits(bits)
    };
    let Some(old_target) = (!old_target_ffi.is_null())
        .then(|| Entity::try_from_bits(old_target_ffi.to_bits()))
        .flatten()
        .filter(|&t| t != Entity::PLACEHOLDER)
    else {
        return;
    };

    let source_ffi = FfiEntity::from_bits(ctx.entity.to_bits());
    let became_empty = if let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() {
        if let Some(vec) = map.rel_targets.get_mut(&(tgt_cid, old_target)) {
            vec.retain(|&e| e != source_ffi);
            if vec.is_empty() {
                map.rel_targets.remove(&(tgt_cid, old_target));
                true
            } else {
                false
            }
        } else {
            false
        }
    } else {
        false
    };
    if became_empty && world.entities().contains(old_target) {
        world.commands().queue(move |w: &mut World| {
            let still_empty = w
                .get_resource::<DynamicComponentHookMap>()
                .is_some_and(|m| m.relationship_targets_for(tgt_cid, old_target).is_empty());
            if still_empty
                && let Ok(mut target_ent) = w.get_entity_mut(old_target)
                && target_ent.contains_id(tgt_cid)
            {
                target_ent.remove_by_id(tgt_cid);
            }
        });
    }
}

fn dynamic_rel_target_on_despawn(world: &mut DeferredWorld<'_>, ctx: HookContext) {
    let (rel_cid, sources) = {
        let Some(mut map) = world.get_resource_mut::<DynamicComponentHookMap>() else {
            return;
        };
        let Some(rel_cid) = map.target_to_rel.get(&ctx.component_id).copied() else {
            return;
        };
        let sources = map
            .rel_targets
            .remove(&(ctx.component_id, ctx.entity))
            .unwrap_or_default();
        (rel_cid, sources)
    };
    if sources.is_empty() {
        return;
    }
    let expected_bits = ctx.entity.to_bits();
    world.commands().queue(move |w: &mut World| {
        for src_ffi in sources {
            let Some(src_ent) = (!src_ffi.is_null())
                .then(|| Entity::try_from_bits(src_ffi.to_bits()))
                .flatten()
                .filter(|&e| e != Entity::PLACEHOLDER)
            else {
                continue;
            };
            let matches = w
                .get_entity(src_ent)
                .ok()
                .and_then(|ent| ent.get_by_id(rel_cid).ok())
                .is_some_and(|ptr| {
                    // SAFETY: `rel_cid` stores an 8-byte `FfiEntity` target handle at offset 0.
                    unsafe {
                        core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) == expected_bits
                    }
                });
            if matches && let Ok(mut ent_mut) = w.get_entity_mut(src_ent) {
                ent_mut.remove_by_id(rel_cid);
            }
        }
    });
}

fn dynamic_on_insert_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    // Increment unconditionally regardless of `is_active` because `ComponentDescriptor` permanently
    // binds `drop_trampoline::<SLOT>` to `ctx.component_id`, which always decrements `live_values`
    // on despawn / `World::drop`.
    if let Some(slot) = world
        .get_resource::<DynamicComponentHookMap>()
        .and_then(|map| map.drop_slot_by_id.get(&ctx.component_id).copied())
    {
        increment_drop_slot_live_values(slot);
    }
    dynamic_rel_on_insert(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_insert);
}

fn dynamic_on_discard_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    dynamic_rel_on_discard(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_discard);
}

fn dynamic_on_remove_hook(world: DeferredWorld<'_>, ctx: HookContext) {
    invoke_dynamic_hook(&world, ctx, |e| e.on_remove);
}

fn dynamic_on_despawn_hook(mut world: DeferredWorld<'_>, ctx: HookContext) {
    dynamic_rel_target_on_despawn(&mut world, ctx);
    invoke_dynamic_hook(&world, ctx, |e| e.on_despawn);
}

// ============================================================================
// Per-World Disarmed-on-Unload Custom Component Clone Runner (`clone_fn`)
// ============================================================================

fn dynamic_clone_runner(_source: &SourceComponent, ctx: &mut ComponentCloneCtx) {
    let comp_id = ctx.component_id();
    let source_entity = ctx.source();
    let target_entity = ctx.target();
    let expected_layout = ctx.component_info().layout();

    ctx.queue_deferred(move |world: &mut World, _mapper| {
        let active_entry = world
            .get_resource::<DynamicComponentHookMap>()
            .and_then(|map| map.clone_by_id.get(&comp_id))
            .and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|e| e.is_active.load(Ordering::Acquire))
            })
            .filter(|entry| entry.layout == expected_layout)
            .map(|entry| (entry.clone_fn, entry.user_data));

        let Some((GuestFnPtr(clone_fn), user_data)) = active_entry else {
            return;
        };

        if world.get_entity(target_entity).is_err() {
            return;
        }

        let size = expected_layout.size();
        if size == 0 {
            if !world
                .get_entity(source_entity)
                .is_ok_and(|ent| ent.contains_id(comp_id))
            {
                return;
            }
            // SAFETY: `is_active` was verified true (`Ordering::Acquire`) and `size == 0`, so
            // `clone_fn` receives valid non-null dangling ZST pointers without reading/writing.
            let status = unsafe {
                clone_fn(
                    core::ptr::NonNull::<u8>::dangling()
                        .as_ptr()
                        .cast::<c_void>(),
                    core::ptr::NonNull::<u8>::dangling()
                        .as_ptr()
                        .cast::<c_void>(),
                    user_data.as_ptr(),
                )
            };
            if status.is_ok()
                && let Ok(mut target_ent) = world.get_entity_mut(target_entity)
            {
                // SAFETY: `size == 0`; `NonNull::dangling()` is valid for ZST `OwningPtr`.
                let owning = unsafe { OwningPtr::new(core::ptr::NonNull::<u8>::dangling()) };
                // SAFETY: `comp_id` was registered in `world` with zero-sized `expected_layout`.
                unsafe {
                    target_ent.insert_by_id(comp_id, owning);
                }
            }
            return;
        }

        let temp = OwnedZeroedBuffer::new(expected_layout);
        let status = {
            let Ok(source_ent) = world.get_entity(source_entity) else {
                return;
            };
            let Ok(src_ptr) = source_ent.get_by_id(comp_id) else {
                return;
            };
            // SAFETY: `src_ptr` points to `size` initialized source bytes; `temp.ptr` points to
            // `size` writable `expected_layout.align()`-aligned bytes; `is_active` is true.
            unsafe {
                clone_fn(
                    src_ptr.as_ptr().cast::<c_void>(),
                    temp.ptr.cast::<c_void>(),
                    user_data.as_ptr(),
                )
            }
        };

        if status.is_ok()
            && let Ok(mut target_ent) = world.get_entity_mut(target_entity)
        {
            // SAFETY: `OwnedZeroedBuffer::new` aborts on null when `size > 0`.
            let nn = unsafe { core::ptr::NonNull::new_unchecked(temp.ptr) };
            // SAFETY: `nn` points to `size` initialized bytes written by `clone_fn`;
            // `insert_by_id` moves out of `owning` without freeing `temp`'s scratch buffer.
            let owning = unsafe { OwningPtr::new(nn) };
            // SAFETY: `comp_id` was registered in `world` with `expected_layout`.
            unsafe {
                target_ent.insert_by_id(comp_id, owning);
            }
        }
    });
}

/// Concrete `#[repr(C)]` initialization context backing opaque [`BamLoadCtx`].
///
/// Valid strictly for the duration of `bevy_abi_mod_init`; poisoned automatically on `finish_init`
/// and `Drop` without any thread-local global state.
#[repr(C)]
pub struct LoadContext {
    /// Leading handle validation header (`LOAD_CTX_MAGIC`) at byte offset `0`.
    pub header: HandleHeader,
    /// Embedded world-scoped `HostContext` returned by `host_load_host_ctx`.
    pub host_ctx: HostContext,
    /// Mutable Copy-on-Write [`ModRegistry`] updated as dynamic components register.
    pub registry: ModRegistry,
    /// Accumulated systems registered during `bevy_abi_mod_init`.
    pub registered_systems: Vec<GuestSystem>,
    /// Accumulated dynamic observers registered during `bevy_abi_mod_init`.
    pub registered_observers: Vec<GuestObserverBinding>,
    /// Handles of dynamic components registered during this init session.
    pub registered_components: Vec<BamComponent>,
    /// Shared liveness flag for this module load session.
    pub is_active: Arc<AtomicBool>,
    /// Pointer to the exclusively borrowed Bevy [`World`] (or null in standalone unit tests).
    pub world_ptr: *mut World,
    /// Fallback standalone `World` used only when `world_ptr` is null in isolated FFI unit tests.
    standalone_world: Option<Box<World>>,
    /// Assigned [`ModId`] for this load session.
    pub mod_id: Option<ModId>,
    /// Optional manifest module name used to prefix unqualified guest component names.
    pub manifest_name: Option<Arc<str>>,
    pre_snapshot: Arc<RegistrySnapshot>,
    staged_drop_bindings: Arc<Mutex<Vec<StagedDropBinding>>>,
    staged_hook_bindings: Arc<Mutex<Vec<StagedHookBinding>>>,
    fresh_dynamic_comp_ids: Vec<ComponentId>,
    fresh_rel_bindings: Vec<(ComponentId, ComponentId)>,
    in_init: bool,
}

const _: () = assert!(core::mem::offset_of!(LoadContext, header) == 0);

impl LoadContext {
    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, and `mod_id`.
    pub fn with_world_and_mod_id(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
    ) -> Self {
        Self::with_manifest_name(registry, is_active, world_ptr, mod_id, None)
    }

    /// Creates an active [`LoadContext`] bound to `registry`, `is_active`, `world_ptr`, `mod_id`,
    /// and canonical `manifest_name` (`"<manifest.name>::<Name>"` prefixing for unqualified components).
    pub fn with_manifest_name(
        registry: ModRegistry,
        is_active: Arc<AtomicBool>,
        world_ptr: *mut World,
        mod_id: Option<ModId>,
        manifest_name: Option<Arc<str>>,
    ) -> Self {
        let pre_snapshot = registry.snapshot_arc();
        let host_ctx =
            HostContext::new(Arc::clone(&pre_snapshot)).with_namespace(manifest_name.clone());
        let mut ctx = Self {
            header: HandleHeader::new(LOAD_CTX_MAGIC),
            host_ctx,
            registry,
            registered_systems: Vec::new(),
            registered_observers: Vec::new(),
            registered_components: Vec::new(),
            is_active,
            world_ptr,
            standalone_world: None,
            mod_id,
            manifest_name,
            pre_snapshot,
            staged_drop_bindings: Arc::new(Mutex::new(Vec::new())),
            staged_hook_bindings: Arc::new(Mutex::new(Vec::new())),
            fresh_dynamic_comp_ids: Vec::new(),
            fresh_rel_bindings: Vec::new(),
            in_init: true,
        };
        if !ctx.world_ptr.is_null()
            && (ctx.mod_id.is_some()
                || ctx
                    .registry
                    .get_by_name(bevy_abi_mod_sys::BAM_CHILD_OF_NAME)
                    .is_some())
        {
            let children_id = ctx
                .world_mut()
                .register_component::<bevy_ecs::hierarchy::Children>();
            if ctx
                .registry
                .get_by_name(bevy_abi_mod_sys::BAM_CHILDREN_NAME)
                .is_none()
            {
                let _ = ctx.registry.register_children_hierarchy(children_id);
                let updated_reg = ctx.registry.clone();
                if let Some(mut world_reg) = ctx.world_mut().get_resource_mut::<ModRegistry>() {
                    *world_reg = updated_reg;
                }
                let new_snap = ctx.registry.snapshot_arc();
                ctx.pre_snapshot = Arc::clone(&new_snap);
                ctx.host_ctx.set_snapshot(new_snap);
            }
        }
        ctx
    }

    /// Shares the transaction's [`StagedDropBinding`] and [`StagedHookBinding`] logs so
    /// `LoadTransaction::drop` automatically rolls back every trampoline mutation recorded during `bevy_abi_mod_init`.
    pub(crate) fn with_staged_drop_bindings(
        mut self,
        staged: Arc<Mutex<Vec<StagedDropBinding>>>,
    ) -> Self {
        self.staged_drop_bindings = staged;
        self
    }

    pub(crate) fn with_staged_hook_bindings(
        mut self,
        staged_hooks: Arc<Mutex<Vec<StagedHookBinding>>>,
    ) -> Self {
        self.staged_hook_bindings = staged_hooks;
        self
    }

    /// Creates an active standalone [`LoadContext`] for unit testing without an external `World`.
    pub fn new(registry: ModRegistry) -> Self {
        Self::with_world_and_mod_id(
            registry,
            Arc::new(AtomicBool::new(true)),
            core::ptr::null_mut(),
            None,
        )
    }

    /// Ends the active initialization window, poisoning both `LoadContext` and its embedded
    /// `HostContext` so any stashed FFI pointers fail validation immediately.
    #[inline]
    pub fn finish_init(&mut self) {
        self.in_init = false;
        self.header.poison();
        self.host_ctx.header.poison();
    }

    /// Rolls back all drop trampoline bindings and registry mutations recorded in this `LoadContext`,
    /// returning freshly allocated trampoline slots to `FREE_LIST` and
    /// restoring the initial `RegistrySnapshot`.
    pub fn rollback(&mut self) {
        self.finish_init();
        let bindings = {
            let mut guard = self
                .staged_drop_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let hook_bindings = {
            let mut guard = self
                .staged_hook_bindings
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            core::mem::take(&mut *guard)
        };
        let fresh_comp_ids = core::mem::take(&mut self.fresh_dynamic_comp_ids);
        let fresh_rels = core::mem::take(&mut self.fresh_rel_bindings);
        let active_guard = Arc::clone(&self.is_active);
        if let Some(mut drop_guard) = self.world_mut().get_resource_mut::<WorldDropGuard>() {
            drop_guard.rollback_staged_bindings(&bindings);
            drop_guard.rollback_staged_hook_bindings(&hook_bindings);
        }
        if let Some(mut hook_map) = self
            .world_mut()
            .get_resource_mut::<DynamicComponentHookMap>()
        {
            hook_map.rollback_for_guard(&active_guard, &fresh_comp_ids, &fresh_rels);
        }
        prune_drop_trampolines_for_guard(&self.is_active);
        let pre = Arc::clone(&self.pre_snapshot);
        self.registry.restore_snapshot(Arc::clone(&pre));
        self.host_ctx.set_snapshot(Arc::clone(&pre));
        if !self.world_ptr.is_null()
            && let Some(mut world_reg) = self.world_mut().get_resource_mut::<ModRegistry>()
        {
            world_reg.restore_snapshot(pre);
        }
        self.registered_components.clear();
        self.registered_systems.clear();
    }

    /// Casts `&mut self` to an opaque `*mut BamLoadCtx` FFI pointer.
    #[inline]
    pub fn as_ffi_ptr(&mut self) -> *mut BamLoadCtx {
        (self as *mut Self).cast::<BamLoadCtx>()
    }

    fn world_mut(&mut self) -> &mut World {
        if !self.world_ptr.is_null() {
            // SAFETY: `self.world_ptr` comes from an exclusively borrowed `&mut World` held for the
            // duration of `bevy_abi_mod_init` with no aliasing references.
            unsafe { &mut *self.world_ptr }
        } else {
            self.standalone_world
                .get_or_insert_with(|| Box::new(World::new()))
        }
    }
}

impl Drop for LoadContext {
    #[inline]
    fn drop(&mut self) {
        self.in_init = false;
        self.header.poison();
    }
}

#[inline]
pub(crate) unsafe fn validate_active_load_ctx<'a>(
    load_ctx: *mut BamLoadCtx,
) -> Result<&'a mut LoadContext, FfiResult> {
    // SAFETY: Unconditionally validates non-null, alignment, and `LOAD_CTX_MAGIC` + non-zero generation.
    let ctx = unsafe {
        validate_handle_mut::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, true)?
    };
    if !ctx.in_init {
        return Err(FfiResult::INVALID_STATE);
    }
    Ok(ctx)
}

/// Obtains the world-scoped [`BamHostCtx`] pointer from an active [`BamLoadCtx`].
///
/// # Safety
/// `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
pub unsafe extern "C" fn host_load_host_ctx(load_ctx: *const BamLoadCtx) -> *const BamHostCtx {
    // SAFETY: Unconditionally validates `load_ctx` against `LOAD_CTX_MAGIC`.
    let Ok(ctx) = (unsafe {
        validate_handle_ref::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, true)
    }) else {
        return core::ptr::null();
    };
    if !ctx.in_init {
        return core::ptr::null();
    }
    ctx.host_ctx.as_ffi_ptr()
}

/// Resolves an existing component or resource by canonical `name`, `uuid`, or both.
///
/// # Safety
/// - `ctx` must point to an active [`HostContext`] cast to [`BamHostCtx`], or null.
/// - If non-null, `name` must point to a NUL-terminated C string.
/// - If non-null, `uuid` must point to a readable [`ComponentUuid`].
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_resolve_component(
    ctx: *const BamHostCtx,
    name: *const c_char,
    uuid: *const ComponentUuid,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Unconditionally validates `ctx` against `HOST_CTX_MAGIC`.
    let host_ctx = match unsafe {
        validate_handle_ref::<HostContext>(ctx.cast::<c_void>(), HOST_CTX_MAGIC, true)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };

    if name.is_null() && uuid.is_null() {
        host_ctx.set_last_error("resolve_component requires at least one of `name` or `uuid`");
        return FfiResult::INVALID_ARGUMENT;
    }

    let parsed_name = if !name.is_null() {
        // SAFETY: Caller guarantees `name` points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            host_ctx.set_last_error("resolve_component `name` is not valid UTF-8");
            return FfiResult::INVALID_UTF8;
        };
        if s.is_empty() {
            host_ctx.set_last_error("resolve_component `name` must not be empty");
            return FfiResult::INVALID_ARGUMENT;
        }
        Some(s)
    } else {
        None
    };

    let parsed_uuid = if !uuid.is_null() {
        // SAFETY: Caller guarantees `uuid` points to a readable `ComponentUuid`.
        Some(unsafe { core::ptr::read_unaligned(uuid) })
    } else {
        None
    };

    let snap = host_ctx.snapshot();
    let resolve_by_name = |raw_name: &str| {
        if let Some(found) = snap.get_by_name(raw_name) {
            return Some(found);
        }
        if !raw_name.contains("::")
            && let Some(ns) = host_ctx.default_namespace()
        {
            let qualified = format!("{ns}::{raw_name}");
            return snap.get_by_name(&qualified);
        }
        None
    };

    let entry = match (parsed_name, parsed_uuid) {
        (Some(n), Some(u)) => match resolve_by_name(n) {
            Some(by_name) => {
                if by_name.uuid != u {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID mismatch for '{n}': expected {:?}, got {u:?}",
                        by_name.uuid
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                by_name
            }
            None => {
                if let Some(by_uuid) = snap.get_by_uuid(&u) {
                    host_ctx.set_last_error(format!(
                        "resolve_component UUID {u:?} is bound to '{}', not '{n}'",
                        by_uuid.name
                    ));
                    return FfiResult::TYPE_MISMATCH;
                }
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            }
        },
        (Some(n), None) => {
            let Some(by_name) = resolve_by_name(n) else {
                host_ctx.set_last_error(format!("component or resource '{n}' not found"));
                return FfiResult::NOT_FOUND;
            };
            by_name
        }
        (None, Some(u)) => {
            let Some(by_uuid) = snap.get_by_uuid(&u) else {
                host_ctx.set_last_error(format!("component or resource with UUID {u:?} not found"));
                return FfiResult::NOT_FOUND;
            };
            by_uuid
        }
        (None, None) => unreachable!(),
    };

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = entry.handle;
    }
    FfiResult::OK
}

const BAM_REL_BIND_PREFIX: &str = "__bam_rel_bind::";

/// Binds a dynamic `(Relationship, RelationshipTarget)` pair when guest code registers the
/// synthetic `"__bam_rel_bind::<rel_handle>::<target_handle>"` marker descriptor.
///
/// # Safety
/// `out_component` must be a non-null, aligned, writable `*mut BamComponent`.
unsafe fn bind_dynamic_relationship_descriptor(
    ctx: &mut LoadContext,
    pair_spec: &str,
    desc: &ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if desc.size != 0
        || desc.align != 1
        || desc.drop_fn.is_some()
        || desc.clone_fn.is_some()
        || desc.has_hooks()
        || desc.required_component_count != 0
    {
        ctx.host_ctx
            .set_last_error("__bam_rel_bind marker must be a zero-sized hookless descriptor");
        return FfiResult::INVALID_ARGUMENT;
    }
    let Some((rel_str, tgt_str)) = pair_spec.split_once("::") else {
        ctx.host_ctx
            .set_last_error("malformed __bam_rel_bind relationship descriptor");
        return FfiResult::INVALID_ARGUMENT;
    };
    let (Ok(rel_raw), Ok(tgt_raw)) = (rel_str.parse::<u32>(), tgt_str.parse::<u32>()) else {
        ctx.host_ctx
            .set_last_error("malformed __bam_rel_bind component handle integer");
        return FfiResult::INVALID_ARGUMENT;
    };
    if rel_raw == 0 || tgt_raw == 0 {
        return FfiResult::INVALID_COMPONENT;
    }
    let rel_handle = BamComponent::from_raw(rel_raw);
    let tgt_handle = BamComponent::from_raw(tgt_raw);

    let Some(rel_meta) = ctx.registry.get_by_handle(rel_handle).cloned() else {
        ctx.host_ctx.set_last_error(format!(
            "relationship source handle {} is not registered",
            rel_handle.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };
    let Some(tgt_meta) = ctx.registry.get_by_handle(tgt_handle).cloned() else {
        ctx.host_ctx.set_last_error(format!(
            "relationship target handle {} is not registered",
            tgt_handle.0
        ));
        return FfiResult::INVALID_COMPONENT;
    };
    if !rel_meta.is_dynamic
        || !tgt_meta.is_dynamic
        || rel_meta.is_resource()
        || rel_meta.is_message()
        || rel_meta.is_event()
        || tgt_meta.is_resource()
        || tgt_meta.is_message()
        || tgt_meta.is_event()
    {
        ctx.host_ctx.set_last_error(
            "dynamic relationship pairs require dynamic non-resource/non-message/non-event components",
        );
        return FfiResult::PERMISSION_DENIED;
    }
    if rel_meta.size != core::mem::size_of::<FfiEntity>()
        || rel_meta.align != core::mem::align_of::<FfiEntity>()
        || rel_meta.drop_fn.is_some()
    {
        ctx.host_ctx.set_last_error(format!(
            "relationship source '{}' must have 8-byte FfiEntity layout without drop_fn",
            rel_meta.name
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    if tgt_meta.size != 0 || tgt_meta.drop_fn.is_some() {
        ctx.host_ctx.set_last_error(format!(
            "relationship target '{}' must be a zero-sized marker component without drop_fn",
            tgt_meta.name
        ));
        return FfiResult::TYPE_MISMATCH;
    }
    let rel_cid = rel_meta.component_id;
    let tgt_cid = tgt_meta.component_id;
    if rel_cid == tgt_cid {
        ctx.host_ctx
            .set_last_error("relationship source and target components must be distinct");
        return FfiResult::INVALID_ARGUMENT;
    }

    let world = ctx.world_mut();
    ensure_dynamic_hook_map_in_world(world);
    {
        let map = world.resource::<DynamicComponentHookMap>();
        if let Some(&existing_tgt) = map.rel_to_target.get(&rel_cid) {
            if existing_tgt == tgt_cid {
                // SAFETY: Caller guarantees `out_component` is non-null, aligned, and writable.
                unsafe {
                    *out_component = tgt_handle;
                }
                return FfiResult::OK;
            }
            ctx.host_ctx.set_last_error(format!(
                "relationship source '{}' is already bound to a different target component",
                rel_meta.name
            ));
            return FfiResult::TYPE_MISMATCH;
        }
        if map
            .target_to_rel
            .get(&tgt_cid)
            .is_some_and(|&existing_rel| existing_rel != rel_cid)
        {
            ctx.host_ctx.set_last_error(format!(
                "relationship target '{}' is already bound to a different source component",
                tgt_meta.name
            ));
            return FfiResult::TYPE_MISMATCH;
        }
    }

    if world
        .archetypes()
        .iter()
        .any(|a| a.contains(rel_cid) || a.contains(tgt_cid))
    {
        ctx.host_ctx.set_last_error(
            "cannot bind relationship hooks after source or target components exist in archetypes",
        );
        return FfiResult::INVALID_STATE;
    }

    if let Some(hooks) = world.register_component_hooks_by_id(rel_cid) {
        hooks.try_on_insert(dynamic_on_insert_hook);
        hooks.try_on_discard(dynamic_on_discard_hook);
    }
    if let Some(hooks) = world.register_component_hooks_by_id(tgt_cid) {
        hooks.try_on_despawn(dynamic_on_despawn_hook);
    }

    world
        .resource_mut::<DynamicComponentHookMap>()
        .bind_relationship_pair(rel_cid, tgt_cid);
    ctx.fresh_rel_bindings.push((rel_cid, tgt_cid));

    // SAFETY: Caller guarantees `out_component` is non-null, aligned, and writable.
    unsafe {
        *out_component = tgt_handle;
    }
    FfiResult::OK
}

/// Registers a dynamic component described by `desc_ptr` inside `load_ctx` and writes its 1-based
/// [`BamComponent`] handle into `*out_component`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`ComponentDesc`], or null.
/// - `out_component` must point to writable [`BamComponent`] storage, or null.
pub unsafe extern "C" fn host_register_component(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const ComponentDesc,
    out_component: *mut BamComponent,
) -> FfiResult {
    if out_component.is_null() {
        return FfiResult::NULL_POINTER;
    }
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = match unsafe { validate_active_load_ctx(load_ctx) } {
        Ok(c) => c,
        Err(err) => return err,
    };

    // SAFETY: `read_descriptor` checks null and `struct_size >= ComponentDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<ComponentDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid ComponentDesc header ({err})"));
            return err;
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("ComponentDesc validation failed ({val_status})"));
        return val_status;
    }

    let Some(layout) = desc.layout() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc size/align violates C struct layout invariants");
        return FfiResult::INVALID_ARGUMENT;
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(name_str) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` is not valid UTF-8");
        return FfiResult::INVALID_UTF8;
    };
    if name_str.is_empty() {
        ctx.host_ctx
            .set_last_error("ComponentDesc `name` must not be empty");
        return FfiResult::INVALID_ARGUMENT;
    }

    // Prefix unqualified dynamic component names with `"<manifest.name>::<Name>"` unless
    // an unqualified entry of that exact name was already registered on the host.
    let canonical_name: std::borrow::Cow<'_, str> =
        if !name_str.contains("::") && ctx.registry.get_by_name(name_str).is_none() {
            if let Some(ref ns) = ctx.manifest_name {
                std::borrow::Cow::Owned(format!("{ns}::{name_str}"))
            } else {
                std::borrow::Cow::Borrowed(name_str)
            }
        } else {
            std::borrow::Cow::Borrowed(name_str)
        };
    let name_ref = canonical_name.as_ref();

    if let Some(pair_spec) = name_ref.strip_prefix(BAM_REL_BIND_PREFIX) {
        // SAFETY: `out_component` was verified non-null above and caller guarantees writable `BamComponent`.
        return unsafe {
            bind_dynamic_relationship_descriptor(ctx, pair_spec, &desc, out_component)
        };
    }

    // Validate `required_components[0..required_component_count]` handles and default constructors.
    let mut required_handles = Vec::with_capacity(desc.required_component_count);
    if desc.required_component_count > 0 {
        // SAFETY: `desc.validate()` verified `!desc.required_components.is_null()` when count > 0.
        let req_slice = unsafe {
            core::slice::from_raw_parts(desc.required_components, desc.required_component_count)
        };
        for &req_handle in req_slice {
            let Some(req_meta) = ctx.registry.get_by_handle(req_handle) else {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required component handle {} is not registered",
                    req_handle.0
                ));
                return FfiResult::INVALID_COMPONENT;
            };
            if req_meta.is_resource() || req_meta.is_message() || req_meta.is_event() {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required handle '{}' must be a Component, not a Resource/Message/Event",
                    req_meta.name
                ));
                return FfiResult::INVALID_COMPONENT;
            }
            if req_meta.uuid == desc.uuid || req_meta.name.as_ref() == name_ref {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc '{}' cannot require itself",
                    name_ref
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
            let has_default_ctor = req_meta.insert_default_fn.is_some()
                || (req_meta.is_abi_pod
                    && req_meta.drop_fn.is_none()
                    && req_meta.drop_slot.is_none()
                    && (req_meta.is_dynamic || req_meta.insert_raw_fn.is_some()));
            if !has_default_ctor {
                ctx.host_ctx.set_last_error(format!(
                    "ComponentDesc required component '{}' lacks a default or POD zero-init constructor",
                    req_meta.name
                ));
                return FfiResult::PERMISSION_DENIED;
            }
            if !required_handles.contains(&req_handle) {
                required_handles.push(req_handle);
            }
        }
    }
    let required_boxed: Box<[BamComponent]> = required_handles.into_boxed_slice();

    // Validate ABI `0.3` `schema_fingerprint` / `fields` and `required_init_descs` upfront before
    // allocating a Bevy `ComponentDescriptor` or trampoline slot.
    // SAFETY: `desc.validate()` verified `!desc.fields.is_null()` when `desc.field_count > 0`.
    let (schema_fingerprint, extracted_fields, owned_field_strings) =
        match unsafe { extract_owned_dynamic_fields(&desc) } {
            Ok(v) => v,
            Err((err, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return err;
            }
        };

    if desc.covers_slot(
        core::mem::offset_of!(ComponentDesc, required_init_desc_count),
        core::mem::size_of::<usize>(),
    ) && desc.required_init_desc_count > 0
    {
        if desc.required_init_descs.is_null() {
            ctx.host_ctx
                .set_last_error("ComponentDesc `required_init_descs` is null with non-zero count");
            return FfiResult::NULL_POINTER;
        }
        // SAFETY: `desc.required_init_descs` is non-null and caller guarantees `desc.required_init_desc_count`
        // readable `BamRequiredComponent` entries.
        let req_init_slice = unsafe {
            core::slice::from_raw_parts(desc.required_init_descs, desc.required_init_desc_count)
        };
        for (idx, req_init) in req_init_slice.iter().enumerate() {
            if (req_init.struct_size as usize) < BamRequiredComponent::MIN_V0_3_SIZE {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] struct_size ({}) < MIN_V0_3_SIZE ({})",
                    req_init.struct_size,
                    BamRequiredComponent::MIN_V0_3_SIZE
                ));
                return FfiResult::INVALID_VERSION;
            }
            if (req_init.flags & !BAM_REQUIRED_KNOWN_FLAGS) != 0 {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] has unsupported flags 0x{:08X}",
                    req_init.flags
                ));
                return FfiResult::UNSUPPORTED;
            }
            if req_init.reserved0 != 0 {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] non-zero `reserved0` ({})",
                    req_init.reserved0
                ));
                return FfiResult::INVALID_ARGUMENT;
            }
            if (req_init.flags & BAM_REQUIRED_FLAG_CUSTOM_BYTES) != 0
                && req_init.init_size > 0
                && req_init.init_bytes.is_null()
            {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] `init_bytes` is null with non-zero `init_size`"
                ));
                return FfiResult::NULL_POINTER;
            }
            if (req_init.flags & BAM_REQUIRED_FLAG_INIT_FN) != 0 && req_init.init_fn.is_none() {
                ctx.host_ctx.set_last_error(format!(
                    "BamRequiredComponent[{idx}] `init_fn` is null with INIT_FN flag set"
                ));
                return FfiResult::NULL_POINTER;
            }
        }
    }

    let storage_flags = component_flags_to_storage_flags(desc.flags);
    let existing_entry = ctx.registry.get_by_name(name_ref).cloned();
    let existing_comp_id = existing_entry.as_ref().map(|e| e.component_id);

    if let Err(collision) = ctx.registry.check_registration_collision(
        name_ref,
        desc.uuid,
        desc.size,
        desc.align,
        storage_flags,
        existing_comp_id,
    ) {
        ctx.host_ctx.set_last_error(collision.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(drop_mismatch) = ctx
        .registry
        .check_dynamic_drop_compatibility(name_ref, desc.drop_fn.is_some())
    {
        ctx.host_ctx.set_last_error(drop_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }
    if let Err(hook_mismatch) = ctx
        .registry
        .check_dynamic_extended_hook_compatibility(name_ref, &desc)
    {
        ctx.host_ctx.set_last_error(hook_mismatch.to_string());
        return FfiResult::TYPE_MISMATCH;
    }

    let is_active = Arc::clone(&ctx.is_active);
    let owner = ctx.mod_id;
    let staged_log = Arc::clone(&ctx.staged_drop_bindings);
    let _legacy_refs = (
        on_add_hook_for_slot as *const (),
        on_remove_hook_for_slot as *const (),
        WorldDropGuard::bind_or_update_hooks as *const (),
    );
    let has_any_dynamic_hook = desc.has_hooks() || !required_boxed.is_empty();

    let (comp_id, drop_slot, was_fresh_id): (ComponentId, Option<usize>, bool) = {
        let world = ctx.world_mut();
        if !world.contains_resource::<WorldDropGuard>() {
            world.insert_resource(WorldDropGuard::new());
        }

        let (slot_opt, trampoline_opt) = if let Some(drop_fn) = desc.drop_fn {
            let bound = world
                .get_resource_mut::<WorldDropGuard>()
                .and_then(|mut g| g.bind_or_update(desc.uuid, drop_fn, Arc::clone(&is_active)));
            let Some((slot, tramp, staged_binding)) = bound else {
                ctx.host_ctx.set_last_error(
                    "exhausted MAX_DROP_TRAMPOLINES (1024) foreign destructor slots",
                );
                return FfiResult::CAPACITY_EXCEEDED;
            };
            staged_log
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(staged_binding);
            (Some(slot), Some(tramp))
        } else {
            (None, None)
        };

        // Reuse an existing dynamic ComponentId ONLY when its permanently bound drop slot matches
        // `slot_opt`. If the old drop slot was recycled after unload and a different slot is now
        // bound, allocate a fresh `ComponentDescriptor` wired to the new `drop_trampoline::<SLOT>`.
        let reusable_comp_id = existing_entry.as_ref().and_then(|entry| {
            if !entry.is_dynamic || entry.drop_slot == slot_opt {
                Some(entry.component_id)
            } else {
                None
            }
        });

        if let Some(existing_id) = reusable_comp_id {
            (existing_id, slot_opt, false)
        } else {
            let storage_type = if desc.is_sparse_set() {
                StorageType::SparseSet
            } else {
                StorageType::Table
            };
            let clone_behavior = if desc.clone_fn.is_some() {
                ComponentCloneBehavior::Custom(dynamic_clone_runner)
            } else if desc.drop_fn.is_some() {
                ComponentCloneBehavior::Ignore
            } else {
                ComponentCloneBehavior::Default
            };
            // SAFETY: `layout` was verified by `desc.layout()`; `trampoline_opt` follows Bevy's
            // `OwningPtr` drop contract; `storage_type`, `is_summary_tick`, and mutability match `desc.flags`.
            let bevy_desc = unsafe {
                ComponentDescriptor::new_with_layout(
                    name_ref.to_string(),
                    storage_type,
                    layout,
                    trampoline_opt,
                    !desc.is_immutable(),
                    desc.is_summary_tick(),
                    clone_behavior,
                    None,
                )
            };
            let new_id = world.register_component_with_descriptor(bevy_desc);
            if (slot_opt.is_some() || has_any_dynamic_hook)
                && let Some(hooks) = world.register_component_hooks_by_id(new_id)
            {
                if desc.on_add.is_some() || !required_boxed.is_empty() {
                    hooks.on_add(dynamic_on_add_hook);
                }
                if desc.on_insert.is_some() {
                    hooks.on_insert(dynamic_on_insert_hook);
                } else if let Some(slot) = slot_opt {
                    hooks.on_insert(on_insert_hook_for_slot(slot));
                }
                if desc.on_discard.is_some() {
                    hooks.on_discard(dynamic_on_discard_hook);
                }
                if desc.on_remove.is_some() {
                    hooks.on_remove(dynamic_on_remove_hook);
                }
                if desc.on_despawn.is_some() {
                    hooks.on_despawn(dynamic_on_despawn_hook);
                }
            }
            (new_id, slot_opt, true)
        }
    };
    if was_fresh_id {
        ctx.fresh_dynamic_comp_ids.push(comp_id);
    }

    let reqs_arc: Arc<[BamComponent]> = Arc::from(required_boxed.as_ref());
    let handle = match ctx.registry.register_dynamic_component_owned_with_schema(
        comp_id,
        &desc,
        name_ref,
        owner,
        drop_slot,
        None,
        required_boxed,
        schema_fingerprint,
        extracted_fields,
        owned_field_strings,
    ) {
        Ok(h) => h,
        Err(err) => {
            ctx.host_ctx.set_last_error(err.to_string());
            return FfiResult::TYPE_MISMATCH;
        }
    };

    if drop_slot.is_some() || has_any_dynamic_hook || desc.clone_fn.is_some() {
        let mut hook_binding = has_any_dynamic_hook.then(|| DynamicHookBinding {
            on_add: desc.on_add.map(GuestFnPtr),
            on_insert: desc.on_insert.map(GuestFnPtr),
            on_discard: desc.on_discard.map(GuestFnPtr),
            on_remove: desc.on_remove.map(GuestFnPtr),
            on_despawn: desc.on_despawn.map(GuestFnPtr),
            user_data: GuestUserData::new(desc.hook_user_data),
            is_active: Arc::clone(&ctx.is_active),
            required_components: reqs_arc,
            registry: ctx.registry.clone(),
        });
        let clone_entry = desc.clone_fn.map(|clone_fn| CloneBindingEntry {
            clone_fn: GuestFnPtr(clone_fn),
            user_data: GuestUserData::new(desc.hook_user_data),
            layout,
            is_active: Arc::clone(&ctx.is_active),
        });
        let world = ctx.world_mut();
        let mut map = world.get_resource_or_insert_with::<DynamicComponentHookMap>(
            DynamicComponentHookMap::default,
        );
        if let Some(slot) = drop_slot {
            map.set_drop_slot(comp_id, slot);
        }
        if let Some(binding) = hook_binding.take() {
            map.insert_or_update(comp_id, binding);
        }
        if let Some(entry) = clone_entry {
            map.insert_or_update_clone(comp_id, entry);
        }
    }

    ctx.host_ctx.set_snapshot(ctx.registry.snapshot_arc());
    if !ctx.world_ptr.is_null() {
        let updated_reg = ctx.registry.clone();
        ctx.world_mut().insert_resource(updated_reg);
    }
    if !ctx.registered_components.contains(&handle) {
        ctx.registered_components.push(handle);
    }

    // SAFETY: `out_component` was checked non-null above and caller guarantees writable `BamComponent`.
    unsafe {
        *out_component = handle;
    }
    FfiResult::OK
}

/// Parses an array of `count` NUL-terminated UTF-8 C strings and validates ordering target syntax.
///
/// # Safety
/// If `count > 0` and `ptr` is non-null, `ptr` must point to `count` readable `*const c_char` slots.
unsafe fn parse_ordering_targets(
    ptr: *const *const c_char,
    count: usize,
) -> Result<Vec<String>, (FfiResult, String)> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if ptr.is_null() {
        return Err((
            FfiResult::NULL_POINTER,
            "ordering array pointer is null with non-zero count".to_string(),
        ));
    }
    // SAFETY: Caller guarantees `count` readable `*const c_char` elements.
    let slice = unsafe { core::slice::from_raw_parts(ptr, count) };
    let mut out = Vec::with_capacity(count);
    for (idx, &entry) in slice.iter().enumerate() {
        if entry.is_null() {
            return Err((
                FfiResult::NULL_POINTER,
                format!("ordering target at index {idx} is null"),
            ));
        }
        // SAFETY: `entry` is non-null and points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(entry) }).to_str() else {
            return Err((
                FfiResult::INVALID_UTF8,
                format!("ordering target at index {idx} is not valid UTF-8"),
            ));
        };
        if ModNamedSystemSet::parse_target(s, "mod").is_none() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("invalid ordering target syntax '{s}' at index {idx}"),
            ));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`, returning its 1-based [`BamSystem`] handle.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
pub(crate) unsafe fn register_system_inner(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
) -> Result<BamSystem, FfiResult> {
    // SAFETY: Validates active `LoadContext` header and `in_init` state.
    let ctx = unsafe { validate_active_load_ctx(load_ctx) }?;

    // SAFETY: `read_descriptor` checks null and `struct_size >= BamSystemDesc::MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamSystemDesc>(desc_ptr.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamSystemDesc header ({err})"));
            return Err(err);
        }
    };

    let val_status = desc.validate();
    if val_status.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamSystemDesc validation failed ({val_status})"));
        return Err(val_status);
    }

    let Some(callback) = desc.callback else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `callback` is null");
        return Err(FfiResult::NULL_POINTER);
    };

    // SAFETY: `desc.validate()` verified `!desc.name.is_null()`.
    let Ok(sys_name) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` is not valid UTF-8");
        return Err(FfiResult::INVALID_UTF8);
    };
    if sys_name.is_empty() {
        ctx.host_ctx
            .set_last_error("BamSystemDesc `name` must not be empty");
        return Err(FfiResult::INVALID_ARGUMENT);
    }

    // Parse `queries[0..desc.query_count]`.
    let mut query_specs = Vec::with_capacity(desc.query_count);
    if desc.query_count > 0 {
        let base_ptr = desc.queries.cast::<u8>();
        // Determine element stride from the first `BamQueryDesc::struct_size` so forward-extended
        // query arrays step accurately.
        // SAFETY: `desc.validate()` verified `!desc.queries.is_null()` when `query_count > 0`.
        let first_size = unsafe { core::ptr::read_unaligned(base_ptr.cast::<u32>()) } as usize;
        if first_size < BamQueryDesc::MIN_V0_2_SIZE {
            ctx.host_ctx.set_last_error(format!(
                "BamQueryDesc[0] struct_size ({first_size}) < MIN_V0_2_SIZE"
            ));
            return Err(FfiResult::INVALID_VERSION);
        }

        for q_idx in 0..desc.query_count {
            let Some(byte_offset) = q_idx.checked_mul(first_size) else {
                return Err(FfiResult::INVALID_ARGUMENT);
            };
            // SAFETY: Caller guarantees `desc.queries` holds `desc.query_count` contiguous elements.
            let q_ptr = unsafe { base_ptr.add(byte_offset).cast::<c_void>() };
            // SAFETY: Reading extensible `BamQueryDesc` element `q_idx`.
            let q_desc = match unsafe { read_descriptor::<BamQueryDesc>(q_ptr) } {
                Ok(q) => q,
                Err(err) => {
                    ctx.host_ctx
                        .set_last_error(format!("invalid BamQueryDesc[{q_idx}] ({err})"));
                    return Err(err);
                }
            };
            let q_val = q_desc.validate();
            if q_val.is_err() {
                ctx.host_ctx
                    .set_last_error(format!("BamQueryDesc[{q_idx}] validation failed ({q_val})"));
                return Err(q_val);
            }

            let mut terms = Vec::with_capacity(q_desc.term_count);
            for t_idx in 0..q_desc.term_count {
                // SAFETY: `q_desc.validate()` verified `!q_desc.terms.is_null()` when `term_count > 0`.
                let term: BamTermDesc =
                    unsafe { core::ptr::read_unaligned(q_desc.terms.add(t_idx)) };
                terms.push(term);
            }
            query_specs.push(BamQuerySpecOwned {
                flags: q_desc.flags,
                terms,
            });
        }
    }

    if let Err((status, msg)) =
        validate_system_queries_against_snapshot(ctx.registry.snapshot(), &query_specs)
    {
        ctx.host_ctx.set_last_error(msg);
        return Err(status);
    }

    // SAFETY: `desc.validate()` verified `after` and `before` non-null when their counts > 0.
    let after_systems = match unsafe { parse_ordering_targets(desc.after, desc.after_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: Same guarantee for `desc.before[0..desc.before_count]`.
    let before_systems = match unsafe { parse_ordering_targets(desc.before, desc.before_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `in_sets` non-null when `in_set_count > 0`.
    let in_sets = match unsafe { parse_ordering_targets(desc.in_sets, desc.in_set_count) } {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `after_weak` is non-null when `after_weak_count > 0`.
    let after_weak = match unsafe { parse_ordering_targets(desc.after_weak, desc.after_weak_count) }
    {
        Ok(v) => v,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return Err(status);
        }
    };
    // SAFETY: `desc.validate()` verified `before_weak` is non-null when `before_weak_count > 0`.
    let before_weak =
        match unsafe { parse_ordering_targets(desc.before_weak, desc.before_weak_count) } {
            Ok(v) => v,
            Err((status, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return Err(status);
            }
        };
    // SAFETY: `desc.validate()` verified `ambiguous_with` is non-null when `ambiguous_with_count > 0`.
    let ambiguous_with =
        match unsafe { parse_ordering_targets(desc.ambiguous_with, desc.ambiguous_with_count) } {
            Ok(v) => v,
            Err((status, msg)) => {
                ctx.host_ctx.set_last_error(msg);
                return Err(status);
            }
        };

    let run_if = desc.run_if.map(|cb| (cb, desc.run_if_user_data));

    let mut sys = GuestSystem::new(
        sys_name,
        query_specs,
        callback,
        desc.user_data,
        ctx.registry.clone(),
    )
    .with_schedule(desc.schedule)
    .with_main_thread(desc.is_main_thread())
    .with_ordering(after_systems, before_systems)
    .with_weak_ordering(after_weak, before_weak)
    .with_in_sets(in_sets)
    .with_ambiguous_with(ambiguous_with)
    .with_ambiguous_with_all(desc.is_ambiguous_with_all())
    .with_run_if(run_if)
    .with_is_active(Arc::clone(&ctx.is_active));

    if let Some(id) = ctx.mod_id {
        sys.set_mod_id(id);
    }

    let next_raw = u32::try_from(ctx.registered_systems.len() + 1)
        .map_err(|_| FfiResult::CAPACITY_EXCEEDED)?;
    ctx.registered_systems.push(sys);
    Ok(BamSystem::from_raw(next_raw))
}

/// Registers a unified [`BamSystemDesc`] inside `load_ctx`.
///
/// # Safety
/// - `load_ctx` must point to the active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc_ptr` must point to a readable [`BamSystemDesc`], or null.
pub unsafe extern "C" fn host_register_system(
    load_ctx: *mut BamLoadCtx,
    desc_ptr: *const BamSystemDesc,
) -> FfiResult {
    // SAFETY: Caller upholds `load_ctx` and `desc_ptr` FFI contract.
    match unsafe { register_system_inner(load_ctx, desc_ptr) } {
        Ok(_) => FfiResult::OK,
        Err(err) => err,
    }
}

/// Resolves a validated [`BamObserverDesc`] into a [`GuestObserverBinding`].
///
/// # Safety
/// - If `desc.name` is non-null, it must point to a valid NUL-terminated C string.
/// - If `desc.watched_component_count > 0`, `desc.watched_components` must point to
///   `desc.watched_component_count` readable [`BamComponent`] handles.
pub(crate) unsafe fn build_observer_binding_from_desc(
    desc: &BamObserverDesc,
    snapshot: &RegistrySnapshot,
    registry: ModRegistry,
    is_active: Arc<AtomicBool>,
    mod_id: Option<ModId>,
) -> Result<GuestObserverBinding, (FfiResult, String)> {
    let obs_name: Arc<str> = if desc.name.is_null() {
        Arc::from("<unnamed_observer>")
    } else {
        // SAFETY: Caller guarantees `desc.name` points to a NUL-terminated C string.
        let Ok(s) = (unsafe { CStr::from_ptr(desc.name) }).to_str() else {
            return Err((
                FfiResult::INVALID_UTF8,
                "BamObserverDesc `name` is not valid UTF-8".to_string(),
            ));
        };
        if s.is_empty() {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                "BamObserverDesc `name` must not be empty".to_string(),
            ));
        }
        Arc::from(s)
    };

    if desc.callback.is_none() && desc.mut_callback.is_none() {
        return Err((
            FfiResult::NULL_POINTER,
            "BamObserverDesc requires `callback` or `mut_callback`".to_string(),
        ));
    }

    let raw_target = desc.entity();
    let target_entity = if raw_target.is_null() {
        None
    } else {
        let Some(entity) = Entity::try_from_bits(raw_target.to_bits()) else {
            return Err((
                FfiResult::INVALID_ENTITY,
                "BamObserverDesc target entity is invalid".to_string(),
            ));
        };
        if entity == Entity::PLACEHOLDER {
            return Err((
                FfiResult::INVALID_ENTITY,
                "BamObserverDesc target entity is Entity::PLACEHOLDER".to_string(),
            ));
        }
        Some(entity)
    };

    let propagate_relationship = if desc.propagate_relationship.is_valid() {
        let Some(rel_meta) = snapshot.get_by_handle(desc.propagate_relationship) else {
            return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `propagate_relationship` handle is not registered".to_string(),
            ));
        };
        if rel_meta.is_resource() || rel_meta.is_message() {
            return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `propagate_relationship` cannot be a Resource or Message"
                    .to_string(),
            ));
        }
        Some(rel_meta.component_id)
    } else {
        None
    };

    let mut watched_components = Vec::with_capacity(desc.watched_component_count + 1);
    if desc.watched_component_count > 0 {
        if desc.watched_components.is_null() {
            return Err((
                FfiResult::NULL_POINTER,
                "BamObserverDesc `watched_components` is null with non-zero count".to_string(),
            ));
        }
        // SAFETY: Caller guarantees `watched_components` has `watched_component_count` readable items.
        let handles = unsafe {
            core::slice::from_raw_parts(desc.watched_components, desc.watched_component_count)
        };
        for &handle in handles {
            let Some(comp_meta) = snapshot.get_by_handle(handle) else {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "BamObserverDesc watched component handle {} is not registered",
                        handle.0
                    ),
                ));
            };
            if comp_meta.is_resource() || comp_meta.is_message() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    format!(
                        "BamObserverDesc watched handle '{}' cannot be a Resource or Message",
                        comp_meta.name
                    ),
                ));
            }
            if !watched_components.contains(&comp_meta.component_id) {
                watched_components.push(comp_meta.component_id);
            }
        }
    }

    let (event_id, event_key, event_size, synthesizes_ffi_entity_payload, extract_target_fn) =
        if desc.event_kind.is_custom() {
            let Some(meta) = snapshot.get_by_handle(desc.event) else {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc `event` handle is not registered".to_string(),
                ));
            };
            if meta.is_resource() || meta.is_message() {
                return Err((
                FfiResult::INVALID_COMPONENT,
                "BamObserverDesc `event` must refer to an Event, EntityEvent, or component, not a Resource or Message"
                    .to_string(),
            ));
            }
            let key = meta.event_key.unwrap_or_else(|| {
                // SAFETY: `meta.component_id` is valid in `world` and matches `meta`'s registered layout.
                unsafe { EventKey::new(meta.component_id) }
            });
            (
                Some(meta.component_id),
                key,
                meta.size,
                meta.synthesizes_ffi_entity_payload,
                meta.extract_event_target_fn,
            )
        } else if desc.event_kind.is_component_lifecycle() {
            if desc.event.is_valid() {
                let Some(comp_meta) = snapshot.get_by_handle(desc.event) else {
                    return Err((
                        FfiResult::INVALID_COMPONENT,
                        "BamObserverDesc lifecycle `event` component handle is not registered"
                            .to_string(),
                    ));
                };
                if comp_meta.is_resource() || comp_meta.is_message() {
                    return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc lifecycle `event` component cannot be a Resource or Message"
                        .to_string(),
                ));
                }
                if !watched_components.contains(&comp_meta.component_id) {
                    watched_components.push(comp_meta.component_id);
                }
            }
            if watched_components.is_empty() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc lifecycle observer requires at least one watched component"
                        .to_string(),
                ));
            }
            let key = match desc.event_kind {
                BamEventKind::ADD => bevy_ecs::lifecycle::ADD,
                BamEventKind::INSERT => bevy_ecs::lifecycle::INSERT,
                BamEventKind::DISCARD => bevy_ecs::lifecycle::DISCARD,
                BamEventKind::REMOVE => bevy_ecs::lifecycle::REMOVE,
                BamEventKind::DESPAWN => bevy_ecs::lifecycle::DESPAWN,
                _ => unreachable!(),
            };
            (None, key, core::mem::size_of::<FfiEntity>(), true, None)
        } else if desc.event_kind.is_ready() {
            let meta = if desc.event.is_valid() {
                let Some(m) = snapshot.get_by_handle(desc.event) else {
                    return Err((
                        FfiResult::INVALID_COMPONENT,
                        "BamObserverDesc READY `event` handle is not registered".to_string(),
                    ));
                };
                m
            } else if let Some(m) = snapshot
                .get_by_name("bevy_scene::Ready")
                .or_else(|| snapshot.get_by_name("Ready"))
            {
                m
            } else {
                return Err((
                    FfiResult::NOT_FOUND,
                    "BamObserverDesc READY event ('bevy_scene::Ready' / 'Ready') is not registered"
                        .to_string(),
                ));
            };
            if meta.is_resource() || meta.is_message() {
                return Err((
                    FfiResult::INVALID_COMPONENT,
                    "BamObserverDesc READY `event` cannot be a Resource or Message".to_string(),
                ));
            }
            let key = meta.event_key.unwrap_or_else(|| {
                // SAFETY: `meta.component_id` is valid in `world` and matches `meta`'s registered layout.
                unsafe { EventKey::new(meta.component_id) }
            });
            (
                Some(meta.component_id),
                key,
                meta.size,
                true,
                meta.extract_event_target_fn,
            )
        } else {
            return Err((
                FfiResult::INVALID_ARGUMENT,
                format!("unsupported BamEventKind({})", desc.event_kind.0),
            ));
        };

    Ok(GuestObserverBinding {
        name: obs_name,
        event: desc.event,
        event_kind: desc.event_kind,
        event_id,
        event_key,
        event_size,
        synthesizes_ffi_entity_payload,
        target_entity,
        watched_components: watched_components.into_boxed_slice(),
        extract_target_fn,
        callback: desc.callback.map(GuestFnPtr),
        mut_callback: desc.mut_callback.map(GuestFnPtr),
        is_propagating: desc.is_propagating(),
        propagate_relationship,
        user_data: GuestUserData::new(desc.user_data),
        is_active,
        registry,
        mod_id,
    })
}

/// Registers a dynamic Bevy ECS [`Observer`](bevy_ecs::observer::Observer) from a [`BamObserverDesc`]
/// during `bevy_abi_mod_init`.
///
/// # Safety
/// - `load_ctx` must point to an active [`LoadContext`] cast to [`BamLoadCtx`], or null.
/// - `desc` must point to a valid aligned [`BamObserverDesc`] whose `name` (if non-null) is NUL-terminated UTF-8.
pub unsafe extern "C" fn host_register_observer(
    load_ctx: *mut BamLoadCtx,
    desc: *const BamObserverDesc,
) -> FfiResult {
    // SAFETY: Validating `load_ctx` against `LOAD_CTX_MAGIC` under `STRICT_FFI_CHECKS`.
    let ctx = match unsafe {
        validate_handle_mut::<LoadContext>(load_ctx.cast::<c_void>(), LOAD_CTX_MAGIC, false)
    } {
        Ok(c) => c,
        Err(err) => return err,
    };
    if !ctx.in_init {
        ctx.host_ctx
            .set_last_error("host_register_observer called outside bevy_abi_mod_init");
        return FfiResult::INVALID_STATE;
    }
    ctx.host_ctx.clear_last_error();

    if desc.is_null() {
        ctx.host_ctx
            .set_last_error("host_register_observer received null BamObserverDesc pointer");
        return FfiResult::NULL_POINTER;
    }

    // SAFETY: `desc` is non-null; `read_descriptor` checks alignment and `struct_size >= MIN_V0_2_SIZE`.
    let desc = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => {
            ctx.host_ctx
                .set_last_error(format!("invalid BamObserverDesc descriptor ({err})"));
            return err;
        }
    };

    let validation = desc.validate();
    if validation.is_err() {
        ctx.host_ctx
            .set_last_error(format!("BamObserverDesc validation failed ({validation})"));
        return validation;
    }

    // SAFETY: `desc` passed validation; `name` and `watched_components` satisfy caller contract.
    let binding = match unsafe {
        build_observer_binding_from_desc(
            &desc,
            ctx.registry.snapshot(),
            ctx.registry.clone(),
            Arc::clone(&ctx.is_active),
            ctx.mod_id,
        )
    } {
        Ok(b) => b,
        Err((status, msg)) => {
            ctx.host_ctx.set_last_error(msg);
            return status;
        }
    };

    ctx.registered_observers.push(binding);
    FfiResult::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_abi_mod_sys::{
        BAM_COMPONENT_FLAG_SPARSE_SET, BAM_COMPONENT_FLAG_SUMMARY_TICK, ModComponent,
    };
    use bevy_ecs::prelude::Component;
    use core::sync::atomic::AtomicUsize;
    use std::ffi::CString;

    struct HookLog {
        events: Mutex<Vec<(&'static str, FfiEntity, u32)>>,
    }

    unsafe extern "C" fn test_hook_add(
        entity: FfiEntity,
        data: *const c_void,
        user_data: *mut c_void,
    ) {
        // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
        let val = unsafe { *data.cast::<u32>() };
        // SAFETY: `user_data` points to `HookLog`.
        let log = unsafe { &*user_data.cast::<HookLog>() };
        log.events.lock().unwrap().push(("add", entity, val));
    }

    unsafe extern "C" fn test_hook_insert(
        entity: FfiEntity,
        data: *const c_void,
        user_data: *mut c_void,
    ) {
        // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
        let val = unsafe { *data.cast::<u32>() };
        // SAFETY: `user_data` points to `HookLog`.
        let log = unsafe { &*user_data.cast::<HookLog>() };
        log.events.lock().unwrap().push(("insert", entity, val));
    }

    unsafe extern "C" fn test_hook_discard(
        entity: FfiEntity,
        data: *const c_void,
        user_data: *mut c_void,
    ) {
        // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
        let val = unsafe { *data.cast::<u32>() };
        // SAFETY: `user_data` points to `HookLog`.
        let log = unsafe { &*user_data.cast::<HookLog>() };
        log.events.lock().unwrap().push(("discard", entity, val));
    }

    unsafe extern "C" fn test_hook_remove(
        entity: FfiEntity,
        data: *const c_void,
        user_data: *mut c_void,
    ) {
        // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
        let val = unsafe { *data.cast::<u32>() };
        // SAFETY: `user_data` points to `HookLog`.
        let log = unsafe { &*user_data.cast::<HookLog>() };
        log.events.lock().unwrap().push(("remove", entity, val));
    }

    unsafe extern "C" fn test_hook_despawn(
        entity: FfiEntity,
        data: *const c_void,
        user_data: *mut c_void,
    ) {
        // SAFETY: `data` points to `u32` and `user_data` points to `HookLog`.
        let val = unsafe { *data.cast::<u32>() };
        // SAFETY: `user_data` points to `HookLog`.
        let log = unsafe { &*user_data.cast::<HookLog>() };
        log.events.lock().unwrap().push(("despawn", entity, val));
    }

    unsafe extern "C" fn test_clone_u32_doubled(
        src: *const c_void,
        dst: *mut c_void,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `src` is readable `u32`, `dst` is writable `u32`.
        let val = unsafe { *src.cast::<u32>() };
        // SAFETY: `dst` is writable `u32`.
        unsafe { *dst.cast::<u32>() = val.saturating_mul(2) };
        if !user_data.is_null() {
            // SAFETY: `user_data` points to `AtomicUsize`.
            unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
        }
        FfiResult::OK
    }

    unsafe extern "C" fn test_clone_zst(
        _src: *const c_void,
        _dst: *mut c_void,
        user_data: *mut c_void,
    ) -> FfiResult {
        if !user_data.is_null() {
            // SAFETY: `user_data` points to `AtomicUsize`.
            unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
        }
        FfiResult::OK
    }

    #[derive(Component)]
    struct OpaqueHostComp(#[allow(dead_code)] String);

    // Opaque host component metadata only; raw insert disabled.
    impl ModComponent for OpaqueHostComp {
        const CANONICAL_NAME: &'static str = "reg_test::OpaqueHostComp";
        const COMPONENT_CSTR: &'static CStr = c"reg_test::OpaqueHostComp";
        const UUID: ComponentUuid = ComponentUuid::from_canonical_name("reg_test::OpaqueHostComp");
        const IS_RAW_INSERTABLE: bool = false;
        const IS_ABI_POD: bool = false;
        type VTable = ();

        fn vtable() -> &'static Self::VTable {
            &()
        }

        fn symbols() -> &'static [bevy_abi_mod_sys::SymbolEntry] {
            &[]
        }
    }

    #[test]
    fn summary_tick_flag_sets_component_info_and_rejects_sparse_set() {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));
        let mut h_summary = BamComponent::INVALID;
        let mut h_plain = BamComponent::INVALID;
        let mut h_bad = BamComponent::INVALID;

        let mut ctx = LoadContext::with_world_and_mod_id(
            reg,
            active,
            &mut world as *mut World,
            Some(ModId(10)),
        );

        let desc_summary = ComponentDesc::new(
            c"reg_test::WithSummary".as_ptr(),
            ComponentUuid::from_parts("reg_test::WithSummary", 4, 4, 0),
            4,
            4,
        )
        .with_flags(BAM_COMPONENT_FLAG_SUMMARY_TICK);
        let desc_plain = ComponentDesc::new(
            c"reg_test::PlainTable".as_ptr(),
            ComponentUuid::from_parts("reg_test::PlainTable", 4, 4, 0),
            4,
            4,
        );
        let desc_bad = ComponentDesc::new(
            c"reg_test::BadSparseSummary".as_ptr(),
            ComponentUuid::from_parts("reg_test::BadSparseSummary", 4, 4, 0),
            4,
            4,
        )
        .with_flags(BAM_COMPONENT_FLAG_SPARSE_SET | BAM_COMPONENT_FLAG_SUMMARY_TICK);

        // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
        unsafe {
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &desc_summary, &mut h_summary),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &desc_plain, &mut h_plain),
                FfiResult::OK
            );
            assert_eq!(
                host_register_component(ctx.as_ffi_ptr(), &desc_bad, &mut h_bad),
                FfiResult::INVALID_ARGUMENT
            );
        }

        let reg_after = ctx.registry.clone();
        ctx.finish_init();

        let m_sum = reg_after.get_by_handle(h_summary).unwrap();
        let m_plain = reg_after.get_by_handle(h_plain).unwrap();
        assert!(m_sum.is_summary_tick());
        assert!(!m_plain.is_summary_tick());
        assert!(
            world
                .components()
                .get_info(m_sum.component_id)
                .unwrap()
                .summary_tick()
        );
        assert!(
            !world
                .components()
                .get_info(m_plain.component_id)
                .unwrap()
                .summary_tick()
        );
    }

    #[test]
    fn all_five_lifecycle_hooks_post_unload_disarm_and_beyond_1024_cap() {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));
        let log = Box::new(HookLog {
            events: Mutex::new(Vec::new()),
        });
        let log_ptr = (&*log as *const HookLog).cast_mut().cast::<c_void>();

        let mut h_hooked = BamComponent::INVALID;
        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(11)),
            );
            let desc = ComponentDesc::new(
                c"reg_test::FullHooks".as_ptr(),
                ComponentUuid::from_parts("reg_test::FullHooks", 4, 4, 0),
                4,
                4,
            )
            .with_hooks(Some(test_hook_add), Some(test_hook_remove), log_ptr)
            .with_on_insert(test_hook_insert)
            .with_on_discard(test_hook_discard)
            .with_on_despawn(test_hook_despawn);
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let r_main = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h_hooked) };
            assert_eq!(r_main, FfiResult::OK);

            // Register 1,100 additional hooked components in the same World to prove zero 1,024-slot cap.
            let extra_count = if cfg!(miri) { 8 } else { 1100 };
            for idx in 0..extra_count {
                let name = format!("reg_test::ManyHooked_{idx}");
                let cname = CString::new(name.as_str()).unwrap();
                let mut h = BamComponent::INVALID;
                let extra = ComponentDesc::new(
                    cname.as_ptr(),
                    ComponentUuid::from_parts(&name, 4, 4, 0),
                    4,
                    4,
                )
                .with_on_insert(test_hook_insert)
                .with_hook_user_data(log_ptr);
                // SAFETY: Valid `LoadContext` and `ComponentDesc`.
                let r_extra = unsafe { host_register_component(ctx.as_ffi_ptr(), &extra, &mut h) };
                assert_eq!(r_extra, FfiResult::OK);
            }

            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let cid = reg_after.get_by_handle(h_hooked).unwrap().component_id;
        let e1 = world.spawn_empty().id();
        let e1_ffi = FfiEntity::from_bits(e1.to_bits());

        let mut v1: u32 = 10;
        // SAFETY: `cid` matches 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e1).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut v1).cast()),
            );
        }
        let mut v2: u32 = 20;
        // SAFETY: `cid` matches 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e1).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut v2).cast()),
            );
        }
        world.entity_mut(e1).remove_by_id(cid);

        let mut v3: u32 = 30;
        // SAFETY: `cid` matches 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e1).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut v3).cast()),
            );
        }
        world.entity_mut(e1).despawn();

        let recorded = log.events.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                ("add", e1_ffi, 10),
                ("insert", e1_ffi, 10),
                ("discard", e1_ffi, 10),
                ("insert", e1_ffi, 20),
                ("discard", e1_ffi, 20),
                ("remove", e1_ffi, 20),
                ("add", e1_ffi, 30),
                ("insert", e1_ffi, 30),
                ("despawn", e1_ffi, 30),
                ("discard", e1_ffi, 30),
                ("remove", e1_ffi, 30),
            ]
        );
        // Disarm `is_active` and verify zero foreign hooks run afterwards.
        active.store(false, Ordering::Release);
        let e2 = world.spawn_empty().id();
        let mut v4: u32 = 40;
        // SAFETY: `cid` matches 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e2).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut v4).cast()),
            );
        }
        world.entity_mut(e2).despawn();
        assert_eq!(log.events.lock().unwrap().len(), 11);
    }

    #[test]
    fn entity_cloner_custom_clone_fn_non_zst_zst_and_post_unload_disarm() {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));
        let clone_count = Box::new(AtomicUsize::new(0));
        let count_ptr = (&*clone_count as *const AtomicUsize)
            .cast_mut()
            .cast::<c_void>();

        let mut h_val = BamComponent::INVALID;
        let mut h_zst = BamComponent::INVALID;
        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(12)),
            );
            let desc_val = ComponentDesc::new(
                c"reg_test::CloneVal".as_ptr(),
                ComponentUuid::from_parts("reg_test::CloneVal", 4, 4, 0),
                4,
                4,
            )
            .with_clone_fn(test_clone_u32_doubled)
            .with_hook_user_data(count_ptr);
            let desc_zst = ComponentDesc::new(
                c"reg_test::CloneZst".as_ptr(),
                ComponentUuid::from_parts("reg_test::CloneZst", 0, 1, 0),
                0,
                1,
            )
            .with_clone_fn(test_clone_zst)
            .with_hook_user_data(count_ptr);
            // SAFETY: Valid `LoadContext` and `ComponentDesc`s.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &desc_val, &mut h_val),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &desc_zst, &mut h_zst),
                    FfiResult::OK
                );
            }
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let cid_val = reg_after.get_by_handle(h_val).unwrap().component_id;
        let cid_zst = reg_after.get_by_handle(h_zst).unwrap().component_id;
        let e = world.spawn_empty().id();
        let mut raw_val: u32 = 21;
        // SAFETY: `cid_val` has 4-byte `u32` layout and `cid_zst` is ZST.
        unsafe {
            world.entity_mut(e).insert_by_id(
                cid_val,
                OwningPtr::new(core::ptr::NonNull::from(&mut raw_val).cast()),
            );
            world.entity_mut(e).insert_by_id(
                cid_zst,
                OwningPtr::new(core::ptr::NonNull::<u8>::dangling()),
            );
        }

        let cloned = world.entity_mut(e).clone_and_spawn();
        let cloned_ref = world.entity(cloned);
        assert!(cloned_ref.contains_id(cid_zst));
        let cloned_ptr = cloned_ref.get_by_id(cid_val).unwrap();
        // SAFETY: `cloned_ptr` points to 4-byte `u32` initialized by `test_clone_u32_doubled`.
        let cloned_val = unsafe { core::ptr::read_unaligned(cloned_ptr.as_ptr().cast::<u32>()) };
        assert_eq!(cloned_val, 42);
        assert_eq!(clone_count.load(Ordering::Relaxed), 2);

        // Disarm `is_active` and clone again: `clone_fn` must NOT execute!
        active.store(false, Ordering::Release);
        let cloned2 = world.entity_mut(e).clone_and_spawn();
        assert!(!world.entity(cloned2).contains_id(cid_val));
        assert!(!world.entity(cloned2).contains_id(cid_zst));
        assert_eq!(clone_count.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn transitive_required_components_preserves_existing_and_validates_ctors() {
        let mut world = World::new();
        let mut reg = ModRegistry::new();
        let opaque_cid = world.register_component::<OpaqueHostComp>();
        reg.register_component::<OpaqueHostComp>(opaque_cid, Default::default())
            .unwrap();
        let h_opaque = reg.get_by_id(opaque_cid).unwrap().handle;

        let active = Arc::new(AtomicBool::new(true));
        let mut h_c = BamComponent::INVALID;
        let mut h_b = BamComponent::INVALID;
        let mut h_a = BamComponent::INVALID;
        let mut h_fail = BamComponent::INVALID;

        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                active,
                &mut world as *mut World,
                Some(ModId(13)),
            );
            let desc_c = ComponentDesc::new(
                c"req_test::CompC".as_ptr(),
                ComponentUuid::from_parts("req_test::CompC", 4, 4, 0),
                4,
                4,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_c, &mut h_c) };
            assert_eq!(rc, FfiResult::OK);

            let reqs_b = [h_c];
            let desc_b = ComponentDesc::new(
                c"req_test::CompB".as_ptr(),
                ComponentUuid::from_parts("req_test::CompB", 4, 4, 0),
                4,
                4,
            )
            .with_required_components(reqs_b.as_ptr(), reqs_b.len());
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let rb = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut h_b) };
            assert_eq!(rb, FfiResult::OK);

            let reqs_a = [h_b];
            let desc_a = ComponentDesc::new(
                c"req_test::CompA".as_ptr(),
                ComponentUuid::from_parts("req_test::CompA", 4, 4, 0),
                4,
                4,
            )
            .with_required_components(reqs_a.as_ptr(), reqs_a.len());
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let ra = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut h_a) };
            assert_eq!(ra, FfiResult::OK);

            // Requiring an opaque host component without a default constructor must fail with PERMISSION_DENIED.
            let bad_reqs = [h_opaque];
            let desc_bad = ComponentDesc::new(
                c"req_test::BadReq".as_ptr(),
                ComponentUuid::from_parts("req_test::BadReq", 4, 4, 0),
                4,
                4,
            )
            .with_required_components(bad_reqs.as_ptr(), bad_reqs.len());
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let rbad = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_bad, &mut h_fail) };
            assert_eq!(rbad, FfiResult::PERMISSION_DENIED);

            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        assert_eq!(reg_after.required_components(h_a), &[h_b]);
        assert_eq!(reg_after.required_components(h_b), &[h_c]);

        let cid_a = reg_after.get_by_handle(h_a).unwrap().component_id;
        let cid_b = reg_after.get_by_handle(h_b).unwrap().component_id;
        let cid_c = reg_after.get_by_handle(h_c).unwrap().component_id;

        // 1. Inserting A automatically inserts transitive requirements B and C.
        let e1 = world.spawn_empty().id();
        let mut val_a: u32 = 7;
        // SAFETY: `cid_a` has 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e1).insert_by_id(
                cid_a,
                OwningPtr::new(core::ptr::NonNull::from(&mut val_a).cast()),
            );
        }
        assert!(world.entity(e1).contains_id(cid_a));
        assert!(world.entity(e1).contains_id(cid_b));
        assert!(world.entity(e1).contains_id(cid_c));
        let b_ptr = world.entity(e1).get_by_id(cid_b).unwrap().as_ptr();
        let c_ptr = world.entity(e1).get_by_id(cid_c).unwrap().as_ptr();
        // SAFETY: `cid_b` and `cid_c` are zero-initialized `u32`s.
        unsafe {
            assert_eq!(core::ptr::read_unaligned(b_ptr.cast::<u32>()), 0);
            assert_eq!(core::ptr::read_unaligned(c_ptr.cast::<u32>()), 0);
        }

        // 2. Pre-existing B(99) is preserved when A is inserted, while missing C is still inserted.
        let e2 = world.spawn_empty().id();
        let mut val_b: u32 = 99;
        // SAFETY: `cid_b` has 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e2).insert_by_id(
                cid_b,
                OwningPtr::new(core::ptr::NonNull::from(&mut val_b).cast()),
            );
            world.entity_mut(e2).insert_by_id(
                cid_a,
                OwningPtr::new(core::ptr::NonNull::from(&mut val_a).cast()),
            );
        }
        assert!(world.entity(e2).contains_id(cid_c));
        let e2_b_ptr = world.entity(e2).get_by_id(cid_b).unwrap().as_ptr();
        // SAFETY: `cid_b` holds 99.
        unsafe {
            assert_eq!(core::ptr::read_unaligned(e2_b_ptr.cast::<u32>()), 99);
        }
    }

    unsafe extern "C" fn test_drop_atomic_ptr(ptr: *mut c_void) {
        // SAFETY: `ptr` stores an aligned `*const AtomicUsize` pointing to a live counter.
        let counter_ptr = unsafe { core::ptr::read_unaligned(ptr.cast::<*const AtomicUsize>()) };
        if !counter_ptr.is_null() {
            // SAFETY: Test keeps the `Box<AtomicUsize>` alive for the test duration.
            unsafe { (*counter_ptr).fetch_add(1, Ordering::Relaxed) };
        }
    }

    unsafe extern "C" fn test_on_insert_atomic(
        _entity: FfiEntity,
        _data: *const c_void,
        user_data: *mut c_void,
    ) {
        if !user_data.is_null() {
            // SAFETY: `user_data` points to a live `AtomicUsize`.
            unsafe { (*user_data.cast::<AtomicUsize>()).fetch_add(1, Ordering::Relaxed) };
        }
    }

    #[test]
    fn drop_fn_plus_on_insert_active_and_disarmed_never_underflows_live_values() {
        let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let drop_count = Box::new(AtomicUsize::new(0));
        let insert_count = Box::new(AtomicUsize::new(0));
        let drop_ptr = &*drop_count as *const AtomicUsize;
        let insert_ptr = (&*insert_count as *const AtomicUsize)
            .cast_mut()
            .cast::<c_void>();

        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));
        let mut handle = BamComponent::INVALID;

        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(21)),
            );
            let desc = ComponentDesc::new(
                c"reg_test::DropPlusInsert".as_ptr(),
                ComponentUuid::from_parts(
                    "reg_test::DropPlusInsert",
                    core::mem::size_of::<*const AtomicUsize>(),
                    core::mem::align_of::<*const AtomicUsize>(),
                    0,
                ),
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
            )
            .with_drop(test_drop_atomic_ptr)
            .with_on_insert(test_on_insert_atomic)
            .with_hook_user_data(insert_ptr);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut handle) };
            assert_eq!(rc, FfiResult::OK);
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let meta = reg_after.get_by_handle(handle).unwrap();
        let cid = meta.component_id;
        let slot = meta.drop_slot.expect("drop_fn allocates a drop slot");
        assert_eq!(
            world
                .get_resource::<DynamicComponentHookMap>()
                .and_then(|m| m.drop_slot_by_id.get(&cid).copied()),
            Some(slot)
        );

        // 1. Active insert + despawn runs both `on_insert` and `drop_fn`.
        let e1 = world.spawn_empty().id();
        let mut payload1 = drop_ptr;
        // SAFETY: `cid` matches `*const AtomicUsize` layout.
        unsafe {
            world.entity_mut(e1).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut payload1).cast()),
            );
        }
        assert_eq!(insert_count.load(Ordering::Relaxed), 1);
        assert!(world.despawn(e1));
        assert_eq!(drop_count.load(Ordering::Relaxed), 1);

        // 2. Disarm `is_active` and insert/despawn again: callbacks do NOT run, and `live_values`
        // must still increment on insert so `drop_trampoline` does not underflow on despawn.
        active.store(false, Ordering::Release);
        let e2 = world.spawn_empty().id();
        let mut payload2 = drop_ptr;
        // SAFETY: `cid` matches `*const AtomicUsize` layout.
        unsafe {
            world.entity_mut(e2).insert_by_id(
                cid,
                OwningPtr::new(core::ptr::NonNull::from(&mut payload2).cast()),
            );
        }
        assert_eq!(insert_count.load(Ordering::Relaxed), 1);
        assert!(world.despawn(e2));
        assert_eq!(drop_count.load(Ordering::Relaxed), 1);
        drop(world);
    }

    #[test]
    fn clone_fn_supports_over_1024_components_and_failed_inits_in_one_world() {
        let mut world = World::new();
        let reg = ModRegistry::new();

        // 1. 60 failed/rolled-back inits with `clone_fn` + 1,100 live registrations in one World.
        let (rollback_iters, live_iters) = if cfg!(miri) { (4, 8) } else { (60, 1100) };
        for idx in 0..rollback_iters {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::new(AtomicBool::new(true)),
                &mut world as *mut World,
                Some(ModId(300 + idx)),
            );
            let name = format!("clone_cap_test::RolledBack_{idx}");
            let cname = CString::new(name.as_str()).unwrap();
            let mut h = BamComponent::INVALID;
            let desc = ComponentDesc::new(
                cname.as_ptr(),
                ComponentUuid::from_parts(&name, 4, 4, 0),
                4,
                4,
            )
            .with_clone_fn(test_clone_u32_doubled);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) };
            assert_eq!(rc, FfiResult::OK);
            ctx.rollback();
        }

        let active = Arc::new(AtomicBool::new(true));
        let mut last_handle = BamComponent::INVALID;
        let reg_after = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                active,
                &mut world as *mut World,
                Some(ModId(400)),
            );
            for idx in 0..live_iters {
                let name = format!("clone_cap_test::Live_{idx}");
                let cname = CString::new(name.as_str()).unwrap();
                let mut h = BamComponent::INVALID;
                let desc = ComponentDesc::new(
                    cname.as_ptr(),
                    ComponentUuid::from_parts(&name, 4, 4, 0),
                    4,
                    4,
                )
                .with_clone_fn(test_clone_u32_doubled);
                // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
                let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc, &mut h) };
                assert_eq!(rc, FfiResult::OK);
                last_handle = h;
            }
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let cid_last = reg_after.get_by_handle(last_handle).unwrap().component_id;
        let e = world.spawn_empty().id();
        let mut val: u32 = 21;
        // SAFETY: `cid_last` has 4-byte `u32` layout.
        unsafe {
            world.entity_mut(e).insert_by_id(
                cid_last,
                OwningPtr::new(core::ptr::NonNull::from(&mut val).cast()),
            );
        }
        let cloned = world.entity_mut(e).clone_and_spawn();
        let cloned_ptr = world.entity(cloned).get_by_id(cid_last).unwrap();
        // SAFETY: `cloned_ptr` holds `u32` produced by `test_clone_u32_doubled`.
        let cloned_val = unsafe { core::ptr::read_unaligned(cloned_ptr.as_ptr().cast::<u32>()) };
        assert_eq!(cloned_val, 42);
    }

    #[test]
    fn recycled_drop_slot_reregistration_allocates_fresh_component_id() {
        let _serial = crate::drop_trampoline::TEST_DROP_SLAB_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let drop_a_count = Box::new(AtomicUsize::new(0));
        let drop_b_count = Box::new(AtomicUsize::new(0));
        let drop_a_ptr = &*drop_a_count as *const AtomicUsize;
        let drop_b_ptr = &*drop_b_count as *const AtomicUsize;

        let mut world = World::new();
        let reg = ModRegistry::new();
        let active_a1 = Arc::new(AtomicBool::new(true));
        let mut h_a1 = BamComponent::INVALID;

        let reg_after_a1 = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active_a1),
                &mut world as *mut World,
                Some(ModId(501)),
            );
            let desc_a = ComponentDesc::new(
                c"recycle_test::CompA".as_ptr(),
                ComponentUuid::from_parts(
                    "recycle_test::CompA",
                    core::mem::size_of::<*const AtomicUsize>(),
                    core::mem::align_of::<*const AtomicUsize>(),
                    0,
                ),
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
            )
            .with_drop(test_drop_atomic_ptr);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a, &mut h_a1) };
            assert_eq!(rc, FfiResult::OK);
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let meta_a1 = reg_after_a1.get_by_handle(h_a1).unwrap();
        let cid_a1 = meta_a1.component_id;
        let slot_a1 = meta_a1.drop_slot.unwrap();

        // Simulate Mod A unloading with 0 live values and its drop slot being recycled cleanly.
        active_a1.store(false, Ordering::Release);
        prune_drop_trampolines_for_guard(&active_a1);
        drop(world.remove_resource::<WorldDropGuard>());
        world.insert_resource(WorldDropGuard::new());

        // Mod B claims the recycled slot (`slot_a1`).
        let active_b = Arc::new(AtomicBool::new(true));
        let mut h_b = BamComponent::INVALID;
        let reg_after_b = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg_after_a1,
                active_b,
                &mut world as *mut World,
                Some(ModId(502)),
            );
            let desc_b = ComponentDesc::new(
                c"recycle_test::CompB".as_ptr(),
                ComponentUuid::from_parts(
                    "recycle_test::CompB",
                    core::mem::size_of::<*const AtomicUsize>(),
                    core::mem::align_of::<*const AtomicUsize>(),
                    0,
                ),
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
            )
            .with_drop(test_drop_atomic_ptr);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_b, &mut h_b) };
            assert_eq!(rc, FfiResult::OK);
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };
        let _slot_b = reg_after_b.get_by_handle(h_b).unwrap().drop_slot.unwrap();

        // Mod A reloads and gets a DIFFERENT drop slot than `slot_a1` (when Mod B took `slot_a1`).
        let active_a2 = Arc::new(AtomicBool::new(true));
        let mut h_a2 = BamComponent::INVALID;
        let reg_after_a2 = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg_after_b,
                active_a2,
                &mut world as *mut World,
                Some(ModId(503)),
            );
            let desc_a2 = ComponentDesc::new(
                c"recycle_test::CompA".as_ptr(),
                ComponentUuid::from_parts(
                    "recycle_test::CompA",
                    core::mem::size_of::<*const AtomicUsize>(),
                    core::mem::align_of::<*const AtomicUsize>(),
                    0,
                ),
                core::mem::size_of::<*const AtomicUsize>(),
                core::mem::align_of::<*const AtomicUsize>(),
            )
            .with_drop(test_drop_atomic_ptr);
            // SAFETY: Valid `LoadContext`, `ComponentDesc`, and output pointer.
            let rc = unsafe { host_register_component(ctx.as_ffi_ptr(), &desc_a2, &mut h_a2) };
            assert_eq!(rc, FfiResult::OK);
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        assert_eq!(h_a2, h_a1);
        let meta_a2 = reg_after_a2.get_by_handle(h_a2).unwrap();
        let slot_a2 = meta_a2.drop_slot.unwrap();
        assert_ne!(slot_a2, slot_a1);
        assert_ne!(
            meta_a2.component_id, cid_a1,
            "re-registering onto a different drop slot must allocate a fresh ComponentId"
        );
        assert_eq!(
            reg_after_a2.get_by_id(meta_a2.component_id).unwrap().handle,
            h_a2
        );
        assert!(reg_after_a2.get_by_id(cid_a1).is_none());

        let ea = world.spawn_empty().id();
        let mut pa = drop_a_ptr;
        let eb = world.spawn_empty().id();
        let mut pb = drop_b_ptr;
        let cid_b = reg_after_a2.get_by_handle(h_b).unwrap().component_id;
        // SAFETY: Layouts match `*const AtomicUsize`.
        unsafe {
            world.entity_mut(ea).insert_by_id(
                meta_a2.component_id,
                OwningPtr::new(core::ptr::NonNull::from(&mut pa).cast()),
            );
            world.entity_mut(eb).insert_by_id(
                cid_b,
                OwningPtr::new(core::ptr::NonNull::from(&mut pb).cast()),
            );
        }
        assert!(world.despawn(ea));
        assert!(world.despawn(eb));
        assert_eq!(drop_a_count.load(Ordering::Relaxed), 1);
        assert_eq!(drop_b_count.load(Ordering::Relaxed), 1);
    }
}
