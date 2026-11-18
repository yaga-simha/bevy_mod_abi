//! Dynamic observer execution (`ModObserverState`, `guest_observer_runner`), extended deferred
//! trigger (`cmd_trigger_ex`), and runtime observer (`cmd_observe`) entrypoints.

use core::alloc::Layout;
use core::ffi::c_void;
use core::num::NonZeroUsize;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use bevy_abi_mod_sys::{
    BamCommands, BamEventKind, BamObserverDesc, BamTriggerDesc, FfiBool, FfiEntity, FfiResult,
    read_descriptor,
};
use bevy_ecs::{
    component::{Component, ComponentId},
    entity::Entity,
    event::{EntityComponentsTrigger, EntityTrigger, EventKey, GlobalTrigger},
    hierarchy::ChildOf,
    lifecycle::{AddEvent, DespawnEvent, DiscardEvent, InsertEvent, RemoveEvent},
    observer::{Observer, TriggerContext},
    prelude::Resource,
    ptr::PtrMut,
    world::{DeferredWorld, World},
};
use bevy_platform::collections::HashSet;

use crate::{
    ffi::{
        commands::{HostCommands, HostCommandsStorage},
        register::{GuestObserverBinding, build_observer_binding_from_desc},
    },
    handle_header::{COMMANDS_MAGIC, validate_handle_mut},
    loader::{ModHost, ModId},
    registry::{EntityEventTriggerFn, ModRegistry, TriggerEventFn},
    sync_ptr::GuestFnPtr,
};

/// ECS [`Component`] storing the shared [`GuestObserverBinding`] on each spawned guest observer entity.
#[derive(Component)]
pub(crate) struct ModObserverState(pub(crate) Arc<GuestObserverBinding>);

/// Per-`World` transient state for an active [`host_cmd_trigger_ex`] dispatch, conveying the
/// current target entity (for dynamic observers without `extract_target_fn`) and mutable
/// propagation flag across `guest_observer_runner`.
#[derive(Resource, Clone, Copy, Debug)]
struct DynamicTriggerState {
    current_target: Option<Entity>,
    propagate: bool,
}

struct DynamicTriggerGuard<'w> {
    world: &'w mut World,
    previous: Option<DynamicTriggerState>,
}

impl<'w> DynamicTriggerGuard<'w> {
    fn enter(world: &'w mut World, state: DynamicTriggerState) -> Self {
        let previous = world.remove_resource::<DynamicTriggerState>();
        world.insert_resource(state);
        Self { world, previous }
    }

    fn world_mut(&mut self) -> &mut World {
        self.world
    }

    fn propagate(&self) -> bool {
        self.world
            .get_resource::<DynamicTriggerState>()
            .is_some_and(|s| s.propagate)
    }
}

impl Drop for DynamicTriggerGuard<'_> {
    fn drop(&mut self) {
        if let Some(prev) = self.previous {
            self.world.insert_resource(prev);
        } else {
            self.world.remove_resource::<DynamicTriggerState>();
        }
    }
}

struct AlignedPayload {
    ptr: *mut u8,
    layout: Layout,
}

impl AlignedPayload {
    /// # Safety
    /// If `size > 0`, `src` must be non-null and readable for `size` bytes.
    unsafe fn copy_from(src: *const u8, size: usize, align: usize) -> Result<Self, FfiResult> {
        let align = align.max(1);
        let Ok(layout) = Layout::from_size_align(size, align) else {
            return Err(FfiResult::INVALID_ARGUMENT);
        };
        if size == 0 {
            return Ok(Self {
                ptr: core::ptr::null_mut(),
                layout,
            });
        }
        if src.is_null() {
            return Err(FfiResult::NULL_POINTER);
        }
        // SAFETY: `size > 0` and `align` is a valid power of two.
        let dst = unsafe { std::alloc::alloc(layout) };
        if dst.is_null() {
            std::alloc::handle_alloc_error(layout);
        }
        // SAFETY: Caller guarantees `src` is readable for `size` bytes; `dst` is freshly
        // allocated with `size` bytes and does not overlap `src`.
        unsafe {
            core::ptr::copy_nonoverlapping(src, dst, size);
        }
        Ok(Self { ptr: dst, layout })
    }

    fn as_ptr(&self) -> *const u8 {
        if self.layout.size() == 0 {
            let nz = NonZeroUsize::new(self.layout.align()).expect("non-zero alignment");
            NonNull::<u8>::without_provenance(nz).as_ptr()
        } else {
            self.ptr.cast_const()
        }
    }

    fn as_ptr_mut(&mut self) -> PtrMut<'_> {
        let nn = if self.layout.size() == 0 {
            let nz = NonZeroUsize::new(self.layout.align()).expect("non-zero alignment");
            NonNull::<u8>::without_provenance(nz)
        } else {
            // SAFETY: `self.ptr` is non-null when `self.layout.size() > 0`.
            unsafe { NonNull::new_unchecked(self.ptr) }
        };
        // SAFETY: `nn` points to `self.layout.size()` initialized bytes aligned to `self.layout.align()`.
        unsafe { PtrMut::new(nn) }
    }
}

impl Drop for AlignedPayload {
    fn drop(&mut self) {
        if self.layout.size() > 0 && !self.ptr.is_null() {
            // SAFETY: `self.ptr` was allocated with `std::alloc::alloc(self.layout)`.
            unsafe {
                std::alloc::dealloc(self.ptr, self.layout);
            }
        }
    }
}

// SAFETY: `AlignedPayload` uniquely owns its heap buffer of copied POD event bytes.
unsafe impl Send for AlignedPayload {}
// SAFETY: Shared references expose no interior mutability.
unsafe impl Sync for AlignedPayload {}

/// Dynamic [`ObserverRunner`](bevy_ecs::observer::ObserverRunner) dispatching Bevy `Event`,
/// `EntityEvent`, `Ready`, or lifecycle events into a registered guest [`BamObserverFn`] or
/// [`BamObserverMutFn`].
///
/// # Safety
/// - `world` must be the [`DeferredWorld`] owning `observer`.
/// - `event_ptr` must point to an initialized value matching the registered event layout.
pub(crate) unsafe fn guest_observer_runner(
    mut world: DeferredWorld,
    observer: Entity,
    _trigger_ctx: &TriggerContext,
    event_ptr: PtrMut,
    _trigger_ptr: PtrMut,
) {
    let Some(binding) = world
        .get::<ModObserverState>(observer)
        .map(|state| Arc::clone(&state.0))
    else {
        return;
    };
    if !binding.is_active.load(Ordering::Acquire) {
        return;
    }

    let target_ffi = if binding.event_kind.is_component_lifecycle() {
        let entity = match binding.event_kind {
            // SAFETY: `event_ptr` points to the lifecycle event struct matching `binding.event_kind`.
            BamEventKind::ADD => unsafe { &*(event_ptr.as_ptr().cast::<AddEvent>()) }.entity,
            // SAFETY: Same as above (`InsertEvent`).
            BamEventKind::INSERT => unsafe { &*(event_ptr.as_ptr().cast::<InsertEvent>()) }.entity,
            BamEventKind::DISCARD => {
                // SAFETY: Same as above (`DiscardEvent`).
                unsafe { &*(event_ptr.as_ptr().cast::<DiscardEvent>()) }.entity
            }
            // SAFETY: Same as above (`RemoveEvent`).
            BamEventKind::REMOVE => unsafe { &*(event_ptr.as_ptr().cast::<RemoveEvent>()) }.entity,
            BamEventKind::DESPAWN => {
                // SAFETY: Same as above (`DespawnEvent`).
                unsafe { &*(event_ptr.as_ptr().cast::<DespawnEvent>()) }.entity
            }
            _ => Entity::PLACEHOLDER,
        };
        if entity == Entity::PLACEHOLDER {
            FfiEntity::NULL
        } else {
            FfiEntity::from_bits(entity.to_bits())
        }
    } else if let Some(extract_fn) = binding.extract_target_fn {
        // SAFETY: `event_ptr` holds an initialized `EntityEvent` matching `binding.event_id`.
        match unsafe { extract_fn(event_ptr.as_ptr()) } {
            Some(entity) => FfiEntity::from_bits(entity.to_bits()),
            None => FfiEntity::NULL,
        }
    } else if let Some(active_target) = world
        .get_resource::<DynamicTriggerState>()
        .and_then(|s| s.current_target)
    {
        FfiEntity::from_bits(active_target.to_bits())
    } else if let Some(watched) = binding.target_entity {
        FfiEntity::from_bits(watched.to_bits())
    } else {
        FfiEntity::NULL
    };

    let initial_propagate = world
        .get_resource::<DynamicTriggerState>()
        .map_or(binding.is_propagating, |s| s.propagate);
    let mut propagate_ffi = FfiBool::from_bool(initial_propagate);

    let mut storage = HostCommandsStorage::new();
    let snap = binding.registry.latest_snapshot_arc();
    let mut status = FfiResult::OK;
    let mut wire_entity = target_ffi;

    if let Some(GuestFnPtr(mut_cb)) = binding.mut_callback {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap)
            .with_mod_context(binding.mod_id, Some(Arc::clone(&binding.is_active)))
            .with_registry(&binding.registry);
        let mut_data_ptr: *mut u8 =
            if binding.event_kind.is_component_lifecycle() || binding.event_size == 0 {
                core::ptr::null_mut()
            } else if binding.synthesizes_ffi_entity_payload {
                (&raw mut wire_entity).cast::<u8>()
            } else {
                event_ptr.as_ptr()
            };
        // SAFETY: `binding.is_active` is verified true, `mut_data_ptr` is null or valid writable
        // storage for this event, `propagate_ffi` is stack-scoped, and `host_cmds` is poisoned before exit.
        status = unsafe {
            mut_cb(
                target_ffi,
                mut_data_ptr,
                &mut propagate_ffi,
                host_cmds.as_ffi_ptr(),
                binding.user_data.as_ptr(),
            )
        };
    }

    if let Some(mut trigger_state) = world.get_resource_mut::<DynamicTriggerState>()
        && binding.mut_callback.is_some()
    {
        trigger_state.propagate = propagate_ffi.get();
    }

    if status == FfiResult::OK
        && let Some(GuestFnPtr(callback)) = binding.callback
    {
        let cell = world.as_unsafe_world_cell();
        let allocator = cell.entity_allocator();
        let mut host_cmds = HostCommands::new(&mut storage, allocator, &snap)
            .with_mod_context(binding.mod_id, Some(Arc::clone(&binding.is_active)))
            .with_registry(&binding.registry);
        let data_ptr = if binding.synthesizes_ffi_entity_payload
            || binding.event_kind.is_component_lifecycle()
        {
            (&raw const wire_entity).cast::<c_void>()
        } else if binding.event_size == 0 {
            core::ptr::null()
        } else {
            event_ptr.as_ptr().cast::<c_void>()
        };
        // SAFETY: `binding.is_active` is verified true and `host_cmds` is poisoned before exit.
        status = unsafe {
            callback(
                target_ffi,
                data_ptr,
                host_cmds.as_ffi_ptr(),
                binding.user_data.as_ptr(),
            )
        };
    }

    if status == FfiResult::OK && !storage.is_empty() {
        world.commands().queue(move |w: &mut World| {
            storage.apply_to_world(w, &snap);
        });
    } else {
        storage.clear();
    }
}

/// Attaches event key, target entity, and watched components to a dynamic [`Observer`].
fn build_observer_component(binding: &GuestObserverBinding) -> Observer {
    let mut obs = Observer::with_dynamic_runner(guest_observer_runner);
    // SAFETY: `binding.event_key` was resolved/registered in `world` with matching payload layout.
    obs = unsafe { obs.with_event_key(binding.event_key) };
    if let Some(target) = binding.target_entity {
        obs.watch_entity(target);
    }
    if !binding.watched_components.is_empty() {
        obs = obs.with_components(binding.watched_components.iter().copied());
    }
    obs
}

/// Spawns all init-registered [`GuestObserverBinding`]s into `world`, flushing immediately and
/// returning the spawned observer entity IDs so `LoadedMod` can despawn them on unload.
pub(crate) fn spawn_registered_observers_in_world(
    world: &mut World,
    registered_observers: Vec<GuestObserverBinding>,
    registry: &ModRegistry,
    mod_id: ModId,
) -> Vec<Entity> {
    if registered_observers.is_empty() {
        return Vec::new();
    }
    let mut observer_entities = Vec::with_capacity(registered_observers.len());
    for mut obs_binding in registered_observers {
        obs_binding.registry = registry.clone();
        obs_binding.mod_id = Some(mod_id);
        let obs = build_observer_component(&obs_binding);
        let obs_entity = world
            .spawn((obs, ModObserverState(Arc::new(obs_binding))))
            .id();
        observer_entities.push(obs_entity);
    }
    world.flush();
    observer_entities
}

#[derive(Clone, Copy)]
enum PropagateRelationship {
    ChildOf,
    DynamicEntity(ComponentId),
}

fn next_propagation_parent(
    world: &World,
    entity: Entity,
    rel: PropagateRelationship,
) -> Option<Entity> {
    match rel {
        PropagateRelationship::ChildOf => world.get::<ChildOf>(entity).map(ChildOf::parent),
        PropagateRelationship::DynamicEntity(cid) => {
            let ent = world.get_entity(entity).ok()?;
            let ptr = ent.get_by_id(cid).ok()?;
            // SAFETY: Validated at `host_cmd_trigger_ex` call time that `cid` has `size == 8`.
            let bits = unsafe { core::ptr::read_unaligned(ptr.as_ptr().cast::<u64>()) };
            Entity::try_from_bits(bits)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn dispatch_custom_at_target(
    world: &mut World,
    event_key: EventKey,
    entity_trigger_fn: Option<EntityEventTriggerFn>,
    trigger_fn: Option<TriggerEventFn>,
    target: Option<Entity>,
    watched_ids: &[ComponentId],
    payload: &mut AlignedPayload,
    propagate: bool,
) -> bool {
    let mut guard = DynamicTriggerGuard::enter(
        world,
        DynamicTriggerState {
            current_target: target,
            propagate,
        },
    );
    let w = guard.world_mut();
    if let Some(et_fn) = entity_trigger_fn {
        if let Some(t) = target
            && t != Entity::PLACEHOLDER
            && w.entities().contains(t)
        {
            et_fn(w, t);
        }
    } else if !watched_ids.is_empty()
        && let Some(t) = target
    {
        let mut trigger = EntityComponentsTrigger {
            components: watched_ids,
            ..Default::default()
        };
        let event_mut = payload.as_ptr_mut();
        let trigger_mut = PtrMut::from(&mut trigger);
        // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityComponentsTrigger`.
        unsafe {
            w.trigger_dynamic_targets_components(event_key, t, watched_ids, event_mut, trigger_mut);
        }
    } else if let Some(tfn) = trigger_fn {
        // SAFETY: `payload.as_ptr()` holds `meta.size` bytes matching the host event type.
        unsafe {
            let _ = tfn(w, target, payload.as_ptr());
        }
    } else if let Some(t) = target {
        let mut trigger = EntityTrigger;
        let event_mut = payload.as_ptr_mut();
        let trigger_mut = PtrMut::from(&mut trigger);
        // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityTrigger`.
        unsafe {
            w.trigger_dynamic_targets(event_key, t, event_mut, trigger_mut);
        }
    } else {
        let mut trigger = GlobalTrigger;
        let event_mut = payload.as_ptr_mut();
        let trigger_mut = PtrMut::from(&mut trigger);
        // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `GlobalTrigger`.
        unsafe {
            w.trigger_dynamic(event_key, event_mut, trigger_mut);
        }
    }
    guard.propagate()
}

fn dispatch_lifecycle_at_target(
    world: &mut World,
    kind: BamEventKind,
    target: Entity,
    watched_ids: &[ComponentId],
    propagate: bool,
) -> bool {
    let mut guard = DynamicTriggerGuard::enter(
        world,
        DynamicTriggerState {
            current_target: Some(target),
            propagate,
        },
    );
    let w = guard.world_mut();
    let mut trigger = EntityComponentsTrigger {
        components: watched_ids,
        ..Default::default()
    };
    let trigger_mut = PtrMut::from(&mut trigger);
    match kind {
        BamEventKind::ADD => {
            let mut ev = AddEvent { entity: target };
            // SAFETY: `ADD` expects `AddEvent` and `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    bevy_ecs::lifecycle::ADD,
                    target,
                    watched_ids,
                    PtrMut::from(&mut ev),
                    trigger_mut,
                );
            }
        }
        BamEventKind::INSERT => {
            let mut ev = InsertEvent { entity: target };
            // SAFETY: `INSERT` expects `InsertEvent` and `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    bevy_ecs::lifecycle::INSERT,
                    target,
                    watched_ids,
                    PtrMut::from(&mut ev),
                    trigger_mut,
                );
            }
        }
        BamEventKind::DISCARD => {
            let mut ev = DiscardEvent { entity: target };
            // SAFETY: `DISCARD` expects `DiscardEvent` and `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    bevy_ecs::lifecycle::DISCARD,
                    target,
                    watched_ids,
                    PtrMut::from(&mut ev),
                    trigger_mut,
                );
            }
        }
        BamEventKind::REMOVE => {
            let mut ev = RemoveEvent { entity: target };
            // SAFETY: `REMOVE` expects `RemoveEvent` and `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    bevy_ecs::lifecycle::REMOVE,
                    target,
                    watched_ids,
                    PtrMut::from(&mut ev),
                    trigger_mut,
                );
            }
        }
        BamEventKind::DESPAWN => {
            let mut ev = DespawnEvent { entity: target };
            // SAFETY: `DESPAWN` expects `DespawnEvent` and `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    bevy_ecs::lifecycle::DESPAWN,
                    target,
                    watched_ids,
                    PtrMut::from(&mut ev),
                    trigger_mut,
                );
            }
        }
        _ => {}
    }
    guard.propagate()
}

/// Queues an extended deferred trigger described by [`BamTriggerDesc`].
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamTriggerDesc`], or null.
pub unsafe extern "C" fn host_cmd_trigger_ex(
    commands: *mut BamCommands,
    desc: *const BamTriggerDesc,
) -> FfiResult {
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamTriggerDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamTriggerDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }

    let snap = host_cmds.snapshot();
    let raw_target = parsed.entity();
    let mut target_entity = if raw_target.is_null() {
        None
    } else {
        let Some(e) = Entity::try_from_bits(raw_target.to_bits()) else {
            return FfiResult::INVALID_ENTITY;
        };
        if e == Entity::PLACEHOLDER {
            return FfiResult::INVALID_ENTITY;
        }
        Some(e)
    };

    let is_propagating = parsed.is_propagating();
    let prop_rel = if is_propagating {
        if parsed.propagate_relationship.is_valid() {
            let Some(rel_meta) = snap.get_by_handle(parsed.propagate_relationship) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if rel_meta.is_resource() || rel_meta.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            if rel_meta.validates_entity_payload {
                PropagateRelationship::ChildOf
            } else if rel_meta.size == core::mem::size_of::<FfiEntity>()
                && rel_meta.align <= core::mem::align_of::<FfiEntity>()
            {
                PropagateRelationship::DynamicEntity(rel_meta.component_id)
            } else {
                return FfiResult::INVALID_COMPONENT;
            }
        } else {
            PropagateRelationship::ChildOf
        }
    } else {
        PropagateRelationship::ChildOf
    };

    let mut watched_ids = Vec::with_capacity(parsed.watched_component_count + 1);
    if parsed.watched_component_count > 0 {
        // SAFETY: `parsed.validate()` verified `!parsed.watched_components.is_null()`.
        let handles = unsafe {
            core::slice::from_raw_parts(parsed.watched_components, parsed.watched_component_count)
        };
        for &handle in handles {
            let Some(m) = snap.get_by_handle(handle) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if m.is_resource() || m.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            if !watched_ids.contains(&m.component_id) {
                watched_ids.push(m.component_id);
            }
        }
    }

    let kind = parsed.event_kind;
    if kind.is_custom() {
        let Some(meta) = snap.get_by_handle(parsed.event) else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource()
            || meta.is_message()
            || meta.event_key.is_none()
            || meta.drop_fn.is_some()
            || meta.drop_slot.is_some()
        {
            return FfiResult::INVALID_COMPONENT;
        }
        if meta.size != parsed.data_len {
            return FfiResult::INVALID_ARGUMENT;
        }
        if meta.is_entity_event() && target_entity.is_none() {
            if meta.synthesizes_ffi_entity_payload {
                if parsed.data_len != core::mem::size_of::<FfiEntity>() || parsed.data_ptr.is_null()
                {
                    return FfiResult::INVALID_ARGUMENT;
                }
                // SAFETY: `parsed.data_ptr` is non-null and readable for 8 bytes (`size_of::<FfiEntity>()`).
                let bits = unsafe { core::ptr::read_unaligned(parsed.data_ptr.cast::<u64>()) };
                let Some(extracted) = Entity::try_from_bits(bits) else {
                    return FfiResult::INVALID_ENTITY;
                };
                if extracted == Entity::PLACEHOLDER {
                    return FfiResult::INVALID_ENTITY;
                }
                target_entity = Some(extracted);
            } else if let Some(extract_fn) = meta.extract_event_target_fn {
                // SAFETY: `parsed.data_ptr` holds `meta.size` initialized bytes matching `EntityEvent`.
                let Some(extracted) = (unsafe { extract_fn(parsed.data_ptr) }) else {
                    return FfiResult::INVALID_ENTITY;
                };
                if extracted == Entity::PLACEHOLDER {
                    return FfiResult::INVALID_ENTITY;
                }
                target_entity = Some(extracted);
            } else {
                return FfiResult::INVALID_ENTITY;
            }
        }
        if is_propagating && target_entity.is_none() {
            return FfiResult::INVALID_ENTITY;
        }
        // SAFETY: `parsed.data_ptr` is readable for `meta.size` bytes when `meta.size > 0`.
        let mut payload =
            match unsafe { AlignedPayload::copy_from(parsed.data_ptr, meta.size, meta.align) } {
                Ok(p) => p,
                Err(err) => return err,
            };
        let event_key = meta.event_key.expect("checked Some above");
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;

        host_cmds.queue_custom(move |world, _snap| {
            if let Some(t) = target_entity
                && !world.entities().contains(t)
            {
                return;
            }
            if !is_propagating {
                let _ = dispatch_custom_at_target(
                    world,
                    event_key,
                    entity_trigger_fn,
                    trigger_fn,
                    target_entity,
                    &watched_ids,
                    &mut payload,
                    false,
                );
                return;
            }
            let Some(mut current) = target_entity else {
                return;
            };
            let mut visited = HashSet::new();
            visited.insert(current);
            loop {
                let cont = dispatch_custom_at_target(
                    world,
                    event_key,
                    entity_trigger_fn,
                    trigger_fn,
                    Some(current),
                    &watched_ids,
                    &mut payload,
                    true,
                );
                if !cont {
                    break;
                }
                let Some(next) = next_propagation_parent(world, current, prop_rel) else {
                    break;
                };
                if next == Entity::PLACEHOLDER
                    || !world.entities().contains(next)
                    || !visited.insert(next)
                {
                    break;
                }
                current = next;
            }
        });
        return FfiResult::OK;
    }

    if kind.is_component_lifecycle() {
        let Some(start_target) = target_entity else {
            return FfiResult::INVALID_ENTITY;
        };
        if parsed.data_len != 0 {
            return FfiResult::INVALID_ARGUMENT;
        }
        if parsed.event.is_valid() {
            let Some(m) = snap.get_by_handle(parsed.event) else {
                return FfiResult::INVALID_COMPONENT;
            };
            if m.is_resource() || m.is_message() {
                return FfiResult::INVALID_COMPONENT;
            }
            if !watched_ids.contains(&m.component_id) {
                watched_ids.push(m.component_id);
            }
        }
        if watched_ids.is_empty() {
            return FfiResult::INVALID_COMPONENT;
        }
        host_cmds.queue_custom(move |world, _snap| {
            if !world.entities().contains(start_target) {
                return;
            }
            if !is_propagating {
                let _ =
                    dispatch_lifecycle_at_target(world, kind, start_target, &watched_ids, false);
                return;
            }
            let mut current = start_target;
            let mut visited = HashSet::new();
            visited.insert(current);
            loop {
                let cont = dispatch_lifecycle_at_target(world, kind, current, &watched_ids, true);
                if !cont {
                    break;
                }
                let Some(next) = next_propagation_parent(world, current, prop_rel) else {
                    break;
                };
                if next == Entity::PLACEHOLDER
                    || !world.entities().contains(next)
                    || !visited.insert(next)
                {
                    break;
                }
                current = next;
            }
        });
        return FfiResult::OK;
    }

    if kind.is_ready() {
        let meta = if parsed.event.is_valid() {
            let Some(m) = snap.get_by_handle(parsed.event) else {
                return FfiResult::INVALID_COMPONENT;
            };
            m
        } else if let Some(m) = snap
            .get_by_name("bevy_scene::Ready")
            .or_else(|| snap.get_by_name("Ready"))
        {
            m
        } else {
            return FfiResult::NOT_FOUND;
        };
        if meta.is_resource() || meta.is_message() || meta.event_key.is_none() {
            return FfiResult::INVALID_COMPONENT;
        }
        if target_entity.is_none() && parsed.data_len == core::mem::size_of::<FfiEntity>() {
            // SAFETY: `parsed.data_len == 8` and `parsed.validate()` verified non-null `data_ptr`.
            let bits = unsafe { core::ptr::read_unaligned(parsed.data_ptr.cast::<u64>()) };
            let Some(e) = Entity::try_from_bits(bits) else {
                return FfiResult::INVALID_ENTITY;
            };
            if e == Entity::PLACEHOLDER {
                return FfiResult::INVALID_ENTITY;
            }
            target_entity = Some(e);
        } else if parsed.data_len != 0 && parsed.data_len != meta.size {
            return FfiResult::INVALID_ARGUMENT;
        }
        let Some(start_target) = target_entity else {
            return FfiResult::INVALID_ENTITY;
        };
        let event_key = meta.event_key.expect("checked Some above");
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;
        let ffi_bytes = FfiEntity::from_bits(start_target.to_bits()).to_bits();
        let mut payload = match unsafe {
            // SAFETY: `ffi_bytes` is a local 8-byte `u64` matching `FfiEntity` layout.
            AlignedPayload::copy_from(
                (&raw const ffi_bytes).cast::<u8>(),
                core::mem::size_of::<FfiEntity>(),
                core::mem::align_of::<FfiEntity>(),
            )
        } {
            Ok(p) => p,
            Err(err) => return err,
        };

        host_cmds.queue_custom(move |world, _snap| {
            if !world.entities().contains(start_target) {
                return;
            }
            if !is_propagating {
                let _ = dispatch_custom_at_target(
                    world,
                    event_key,
                    entity_trigger_fn,
                    trigger_fn,
                    Some(start_target),
                    &watched_ids,
                    &mut payload,
                    false,
                );
                return;
            }
            let mut current = start_target;
            let mut visited = HashSet::new();
            visited.insert(current);
            loop {
                let cont = dispatch_custom_at_target(
                    world,
                    event_key,
                    entity_trigger_fn,
                    trigger_fn,
                    Some(current),
                    &watched_ids,
                    &mut payload,
                    true,
                );
                if !cont {
                    break;
                }
                let Some(next) = next_propagation_parent(world, current, prop_rel) else {
                    break;
                };
                if next == Entity::PLACEHOLDER
                    || !world.entities().contains(next)
                    || !visited.insert(next)
                {
                    break;
                }
                current = next;
            }
        });
        return FfiResult::OK;
    }

    FfiResult::INVALID_ARGUMENT
}

/// Queues deferred spawning of a runtime observer described by [`BamObserverDesc`], optionally
/// writing the reserved observer [`FfiEntity`] into `*out_entity` when non-null.
///
/// # Safety
/// - `commands` must point to an active [`HostCommands`] cast to [`BamCommands`], or null.
/// - `desc` must point to a readable [`BamObserverDesc`], or null.
/// - When `out_entity` is non-null, it must point to writable [`FfiEntity`] storage.
pub unsafe extern "C" fn host_cmd_observe(
    commands: *mut BamCommands,
    desc: *const BamObserverDesc,
    out_entity: *mut FfiEntity,
) -> FfiResult {
    if !out_entity.is_null() {
        if !out_entity.is_aligned() {
            return FfiResult::INVALID_ARGUMENT;
        }
        // SAFETY: `out_entity` is non-null, aligned, and writable.
        unsafe {
            *out_entity = FfiEntity::NULL;
        }
    }
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let Ok(host_cmds) = (unsafe {
        validate_handle_mut::<HostCommands<'_>>(commands.cast::<c_void>(), COMMANDS_MAGIC, false)
    }) else {
        return if commands.is_null() {
            FfiResult::NULL_POINTER
        } else {
            FfiResult::INVALID_STATE
        };
    };
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamObserverDesc::MIN_V0_2_SIZE`.
    let parsed = match unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) } {
        Ok(d) => d,
        Err(err) => return err,
    };
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }

    let snap = host_cmds.snapshot();
    let registry = host_cmds.registry().cloned().unwrap_or_default();
    let is_active = host_cmds
        .is_active()
        .cloned()
        .unwrap_or_else(|| Arc::new(AtomicBool::new(true)));
    let mod_id = host_cmds.owner_mod_id();

    // SAFETY: `parsed` passed validation; `name` and `watched_components` satisfy caller contract.
    let binding = match unsafe {
        build_observer_binding_from_desc(&parsed, snap, registry, is_active, mod_id)
    } {
        Ok(b) => b,
        Err((status, _msg)) => return status,
    };

    let obs_entity = host_cmds.reserve_entity();
    if !out_entity.is_null() {
        // SAFETY: `out_entity` is non-null, aligned, and writable.
        unsafe {
            *out_entity = FfiEntity::from_bits(obs_entity.to_bits());
        }
    }

    host_cmds.queue_custom(move |world, _snap| {
        if !binding.is_active.load(Ordering::Acquire) {
            return;
        }
        let obs = build_observer_component(&binding);
        if let Ok(mut e) = world.spawn_empty_at(obs_entity) {
            e.insert((obs, ModObserverState(Arc::new(binding))));
        } else {
            return;
        }
        if let Some(id) = mod_id
            && let Some(mut mod_host) = world.get_resource_mut::<ModHost>()
            && let Some(loaded) = mod_host.get_mod_mut(id)
        {
            loaded.observer_entities.push(obs_entity);
        }
        world.flush();
    });

    FfiResult::OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ffi::register::{LoadContext, host_register_component, host_register_observer};
    use bevy_abi_mod_sys::{BamComponent, ComponentDesc, ComponentUuid};
    use bevy_ecs::event::EntityEvent;
    use std::sync::Mutex;

    #[repr(C)]
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct DamagePayload {
        amount: u32,
    }

    #[derive(EntityEvent, Clone, Copy, Debug)]
    struct TestReady {
        entity: Entity,
    }

    impl From<Entity> for TestReady {
        fn from(entity: Entity) -> Self {
            Self { entity }
        }
    }

    struct LogCtx {
        events: Mutex<Vec<(&'static str, FfiEntity)>>,
    }

    struct MutObsCtx {
        delta: u32,
        stop_propagate: bool,
        seen: Mutex<Vec<(FfiEntity, u32)>>,
    }

    unsafe extern "C" fn record_add(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity` and `user_data` points to `LogCtx`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("add", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn record_insert(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("insert", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn record_discard(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("discard", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn record_remove(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("remove", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn record_despawn(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("despawn", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn record_ready(
        entity: FfiEntity,
        data: *const c_void,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `data` points to synthesized `FfiEntity`.
        let wire = unsafe { *data.cast::<FfiEntity>() };
        assert_eq!(wire, entity);
        // SAFETY: `user_data` points to `LogCtx`.
        let log = unsafe { &*user_data.cast::<LogCtx>() };
        log.events.lock().unwrap().push(("ready", entity));
        FfiResult::OK
    }

    unsafe extern "C" fn mut_damage_cb(
        entity: FfiEntity,
        event_data: *mut u8,
        propagate: *mut FfiBool,
        _cmds: *mut BamCommands,
        user_data: *mut c_void,
    ) -> FfiResult {
        // SAFETY: `user_data` points to `MutObsCtx` and `event_data` points to `DamagePayload`.
        let ctx = unsafe { &*user_data.cast::<MutObsCtx>() };
        // SAFETY: `event_data` is readable/writable for `DamagePayload`.
        let mut dmg = unsafe { core::ptr::read_unaligned(event_data.cast::<DamagePayload>()) };
        dmg.amount += ctx.delta;
        // SAFETY: `event_data` is writable for `DamagePayload`.
        unsafe { core::ptr::write_unaligned(event_data.cast::<DamagePayload>(), dmg) };
        ctx.seen.lock().unwrap().push((entity, dmg.amount));
        if ctx.stop_propagate {
            // SAFETY: `propagate` is writable `FfiBool`.
            unsafe {
                *propagate = FfiBool::FALSE;
            }
        }
        FfiResult::OK
    }

    #[test]
    fn lifecycle_observers_add_insert_discard_remove_despawn_and_ready() {
        let mut world = World::new();
        let mut reg = ModRegistry::new();
        reg.register_bridged_entity_event::<TestReady>(
            &mut world,
            "bevy_scene::Ready",
            Some("Ready"),
        )
        .unwrap();
        let active = Arc::new(AtomicBool::new(true));
        let log = Box::new(LogCtx {
            events: Mutex::new(Vec::new()),
        });
        let log_ptr = (&*log as *const LogCtx).cast_mut().cast::<c_void>();

        let mut comp_handle = BamComponent::INVALID;
        let registered_observers = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(1)),
            );
            let comp_desc = ComponentDesc::new(
                c"obs_test::Health".as_ptr(),
                ComponentUuid::from_parts("obs_test::Health", 4, 4, 0),
                4,
                4,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let rc =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &comp_desc, &mut comp_handle) };
            assert_eq!(rc, FfiResult::OK);

            for (kind, cb, name) in [
                (BamEventKind::ADD, record_add as _, c"on_add"),
                (BamEventKind::INSERT, record_insert as _, c"on_insert"),
                (BamEventKind::DISCARD, record_discard as _, c"on_discard"),
                (BamEventKind::REMOVE, record_remove as _, c"on_remove"),
                (BamEventKind::DESPAWN, record_despawn as _, c"on_despawn"),
            ] {
                let desc = BamObserverDesc::new_lifecycle(kind, &comp_handle, 1, cb, log_ptr)
                    .with_name(name.as_ptr());
                // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
                let ro = unsafe { host_register_observer(ctx.as_ffi_ptr(), &desc) };
                assert_eq!(ro, FfiResult::OK);
            }

            let ready_desc = BamObserverDesc::new_lifecycle(
                BamEventKind::READY,
                core::ptr::null(),
                0,
                record_ready,
                log_ptr,
            )
            .with_name(c"on_ready".as_ptr());
            // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
            let rr = unsafe { host_register_observer(ctx.as_ffi_ptr(), &ready_desc) };
            assert_eq!(rr, FfiResult::OK);

            reg = ctx.registry.clone();
            ctx.finish_init();
            core::mem::take(&mut ctx.registered_observers)
        };

        let _obs =
            spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(1));

        let e = world.spawn_empty().id();
        let e_ffi = FfiEntity::from_bits(e.to_bits());
        let comp_id = reg.get_by_handle(comp_handle).unwrap().component_id;

        let mut val: u32 = 10;
        let nn = NonNull::from(&mut val).cast::<u8>();
        // SAFETY: `nn` points to 4 bytes matching `comp_id`.
        unsafe {
            world
                .entity_mut(e)
                .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn));
        }

        let mut val2: u32 = 20;
        let nn2 = NonNull::from(&mut val2).cast::<u8>();
        // SAFETY: `nn2` points to 4 bytes matching `comp_id`.
        unsafe {
            world
                .entity_mut(e)
                .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn2));
        }

        world.entity_mut(e).remove_by_id(comp_id);

        let mut val3: u32 = 30;
        let nn3 = NonNull::from(&mut val3).cast::<u8>();
        // SAFETY: `nn3` points to 4 bytes matching `comp_id`.
        unsafe {
            world
                .entity_mut(e)
                .insert_by_id(comp_id, bevy_ecs::ptr::OwningPtr::new(nn3));
        }
        world.entity_mut(e).despawn();

        // Trigger synthetic READY via `host_cmd_trigger_ex` on a new entity.
        let e2 = world.spawn_empty().id();
        let e2_ffi = FfiEntity::from_bits(e2.to_bits());
        let snap = reg.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(1)), Some(active))
                .with_registry(&reg);
            let trig = BamTriggerDesc::new(BamComponent::INVALID, core::ptr::null_mut(), 0)
                .with_event_kind(BamEventKind::READY)
                .with_entity(e2_ffi);
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage.apply_to_world(&mut world, &snap);

        let recorded = log.events.lock().unwrap().clone();
        assert_eq!(
            recorded,
            vec![
                ("add", e_ffi),
                ("insert", e_ffi),
                ("discard", e_ffi),
                ("insert", e_ffi),
                ("discard", e_ffi),
                ("remove", e_ffi),
                ("add", e_ffi),
                ("insert", e_ffi),
                ("despawn", e_ffi),
                ("discard", e_ffi),
                ("remove", e_ffi),
                ("ready", e2_ffi),
            ]
        );
    }

    #[test]
    fn mutable_observer_and_child_of_propagation_with_stop_and_full_bubble() {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));

        let grandparent = world.spawn_empty().id();
        let parent = world.spawn(ChildOf(grandparent)).id();
        let child = world.spawn(ChildOf(parent)).id();

        let child_ctx = Box::new(MutObsCtx {
            delta: 5,
            stop_propagate: false,
            seen: Mutex::new(Vec::new()),
        });
        let parent_stop_ctx = Box::new(MutObsCtx {
            delta: 1,
            stop_propagate: true,
            seen: Mutex::new(Vec::new()),
        });
        let gp_ctx = Box::new(MutObsCtx {
            delta: 100,
            stop_propagate: false,
            seen: Mutex::new(Vec::new()),
        });

        let mut evt_handle = BamComponent::INVALID;
        let (reg, registered_observers) = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(2)),
            );
            let evt_desc = ComponentDesc::new(
                c"obs_test::DamagePayload".as_ptr(),
                ComponentUuid::from_parts("obs_test::DamagePayload", 4, 4, 0),
                4,
                4,
            );
            // SAFETY: Valid `LoadContext` and `ComponentDesc`.
            let rc =
                unsafe { host_register_component(ctx.as_ffi_ptr(), &evt_desc, &mut evt_handle) };
            assert_eq!(rc, FfiResult::OK);

            for (target, ptr, name) in [
                (
                    child,
                    (&*child_ctx as *const MutObsCtx)
                        .cast_mut()
                        .cast::<c_void>(),
                    c"child_obs",
                ),
                (
                    parent,
                    (&*parent_stop_ctx as *const MutObsCtx)
                        .cast_mut()
                        .cast::<c_void>(),
                    c"parent_obs",
                ),
                (
                    grandparent,
                    (&*gp_ctx as *const MutObsCtx).cast_mut().cast::<c_void>(),
                    c"gp_obs",
                ),
            ] {
                let obs = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, ptr)
                    .with_name(name.as_ptr())
                    .with_entity(FfiEntity::from_bits(target.to_bits()))
                    .with_propagate(BamComponent::INVALID);
                // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
                let ro = unsafe { host_register_observer(ctx.as_ffi_ptr(), &obs) };
                assert_eq!(ro, FfiResult::OK);
            }

            let updated_reg = ctx.registry.clone();
            ctx.finish_init();
            (updated_reg, core::mem::take(&mut ctx.registered_observers))
        };

        let _obs =
            spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(2));

        let snap = reg.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(2)), Some(Arc::clone(&active)))
                .with_registry(&reg);
            let mut payload = DamagePayload { amount: 10 };
            let trig = BamTriggerDesc::new(
                evt_handle,
                (&raw mut payload).cast::<u8>(),
                core::mem::size_of::<DamagePayload>(),
            )
            .with_entity(FfiEntity::from_bits(child.to_bits()))
            .with_propagate(BamComponent::INVALID);
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage.apply_to_world(&mut world, &snap);

        assert_eq!(
            *child_ctx.seen.lock().unwrap(),
            vec![(FfiEntity::from_bits(child.to_bits()), 15)]
        );
        assert_eq!(
            *parent_stop_ctx.seen.lock().unwrap(),
            vec![(FfiEntity::from_bits(parent.to_bits()), 16)]
        );
        // Grandparent must NOT have executed because parent set `*propagate = FfiBool::FALSE`.
        assert!(gp_ctx.seen.lock().unwrap().is_empty());

        // Now remove `parent` from hierarchy propagation by triggering directly on `parent` after
        // adding a second non-stopping parent? Better: detach `parent`'s observer and re-attach a
        // non-stopping parent observer via `host_cmd_observe`, verifying full bubble to `grandparent`!
        let parent_continue_ctx = Box::new(MutObsCtx {
            delta: 2,
            stop_propagate: false,
            seen: Mutex::new(Vec::new()),
        });
        let world_obs_entities: Vec<Entity> = world
            .query::<(Entity, &ModObserverState)>()
            .iter(&world)
            .filter(|(_, st)| st.0.name.as_ref() == "parent_obs")
            .map(|(e, _)| e)
            .collect();
        for e in world_obs_entities {
            world.entity_mut(e).despawn();
        }
        let mut storage2 = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(2)), Some(active))
                .with_registry(&reg);
            let obs = BamObserverDesc::new_mut(
                evt_handle,
                mut_damage_cb,
                (&*parent_continue_ctx as *const MutObsCtx)
                    .cast_mut()
                    .cast::<c_void>(),
            )
            .with_entity(FfiEntity::from_bits(parent.to_bits()))
            .with_propagate(BamComponent::INVALID);
            // SAFETY: Valid `HostCommands` and `BamObserverDesc`.
            let ro = unsafe { host_cmd_observe(cmds.as_ffi_ptr(), &obs, core::ptr::null_mut()) };
            assert_eq!(ro, FfiResult::OK);
            let mut payload = DamagePayload { amount: 20 };
            let trig = BamTriggerDesc::new(
                evt_handle,
                (&raw mut payload).cast::<u8>(),
                core::mem::size_of::<DamagePayload>(),
            )
            .with_entity(FfiEntity::from_bits(child.to_bits()))
            .with_propagate(BamComponent::INVALID);
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage2.apply_to_world(&mut world, &snap);

        assert_eq!(
            *gp_ctx.seen.lock().unwrap(),
            vec![(FfiEntity::from_bits(grandparent.to_bits()), 127)]
        );
    }

    #[test]
    fn custom_relationship_propagation_cycle_guard_and_runtime_observe_disarm() {
        let mut world = World::new();
        let reg = ModRegistry::new();
        let active = Arc::new(AtomicBool::new(true));

        let mut evt_handle = BamComponent::INVALID;
        let mut rel_handle = BamComponent::INVALID;
        let reg = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg,
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(3)),
            );
            let evt_desc = ComponentDesc::new(
                c"obs_test::Ping".as_ptr(),
                ComponentUuid::from_parts("obs_test::Ping", 4, 4, 0),
                4,
                4,
            );
            let rel_desc = ComponentDesc::new(
                c"obs_test::CustomLink".as_ptr(),
                ComponentUuid::from_parts("obs_test::CustomLink", 8, 8, 0),
                8,
                8,
            );
            // SAFETY: Valid `LoadContext` and descriptors.
            unsafe {
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &evt_desc, &mut evt_handle),
                    FfiResult::OK
                );
                assert_eq!(
                    host_register_component(ctx.as_ffi_ptr(), &rel_desc, &mut rel_handle),
                    FfiResult::OK
                );
            }
            let r = ctx.registry.clone();
            ctx.finish_init();
            r
        };

        let a = world.spawn_empty().id();
        let b = world.spawn_empty().id();
        let c_unwatched = world.spawn_empty().id();
        let rel_cid = reg.get_by_handle(rel_handle).unwrap().component_id;
        // Create a cycle: a -> b -> a
        let mut link_to_b = FfiEntity::from_bits(b.to_bits());
        let mut link_to_a = FfiEntity::from_bits(a.to_bits());
        // SAFETY: `rel_cid` has 8-byte `FfiEntity` layout.
        unsafe {
            world.entity_mut(a).insert_by_id(
                rel_cid,
                bevy_ecs::ptr::OwningPtr::new(NonNull::from(&mut link_to_b).cast()),
            );
            world.entity_mut(b).insert_by_id(
                rel_cid,
                bevy_ecs::ptr::OwningPtr::new(NonNull::from(&mut link_to_a).cast()),
            );
        }

        let obs_ctx = Box::new(MutObsCtx {
            delta: 1,
            stop_propagate: false,
            seen: Mutex::new(Vec::new()),
        });
        let obs_ptr = (&*obs_ctx as *const MutObsCtx).cast_mut().cast::<c_void>();

        let snap = reg.snapshot_arc();
        let mut obs_a_out = FfiEntity::NULL;
        let mut obs_b_out = FfiEntity::NULL;
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(3)), Some(Arc::clone(&active)))
                .with_registry(&reg);
            let desc_a = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, obs_ptr)
                .with_entity(FfiEntity::from_bits(a.to_bits()));
            let desc_b = BamObserverDesc::new_mut(evt_handle, mut_damage_cb, obs_ptr)
                .with_entity(FfiEntity::from_bits(b.to_bits()));
            // SAFETY: Valid `HostCommands` and `BamObserverDesc`.
            unsafe {
                assert_eq!(
                    host_cmd_observe(cmds.as_ffi_ptr(), &desc_a, &mut obs_a_out),
                    FfiResult::OK
                );
                assert_eq!(
                    host_cmd_observe(cmds.as_ffi_ptr(), &desc_b, &mut obs_b_out),
                    FfiResult::OK
                );
            }
            assert!(!obs_a_out.is_null());
            assert!(!obs_b_out.is_null());

            let mut payload = DamagePayload { amount: 0 };
            let trig = BamTriggerDesc::new(
                evt_handle,
                (&raw mut payload).cast::<u8>(),
                core::mem::size_of::<DamagePayload>(),
            )
            .with_entity(FfiEntity::from_bits(a.to_bits()))
            .with_propagate(rel_handle);
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage.apply_to_world(&mut world, &snap);

        // Must visit `a` then `b` once and break the cycle cleanly.
        assert_eq!(
            *obs_ctx.seen.lock().unwrap(),
            vec![
                (FfiEntity::from_bits(a.to_bits()), 1),
                (FfiEntity::from_bits(b.to_bits()), 2),
            ]
        );

        // Triggering on unwatched entity `c_unwatched` must not invoke `a`/`b` observers.
        let mut storage2 = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(3)), Some(Arc::clone(&active)))
                .with_registry(&reg);
            let mut payload = DamagePayload { amount: 5 };
            let trig = BamTriggerDesc::new(
                evt_handle,
                (&raw mut payload).cast::<u8>(),
                core::mem::size_of::<DamagePayload>(),
            )
            .with_entity(FfiEntity::from_bits(c_unwatched.to_bits()));
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage2.apply_to_world(&mut world, &snap);
        assert_eq!(obs_ctx.seen.lock().unwrap().len(), 2);

        // Disarming `is_active` (as during mod unload) prevents runtime observers from executing.
        active.store(false, Ordering::Release);
        let mut storage3 = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage3, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(3)), Some(active))
                .with_registry(&reg);
            let mut payload = DamagePayload { amount: 5 };
            let trig = BamTriggerDesc::new(
                evt_handle,
                (&raw mut payload).cast::<u8>(),
                core::mem::size_of::<DamagePayload>(),
            )
            .with_entity(FfiEntity::from_bits(a.to_bits()));
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage3.apply_to_world(&mut world, &snap);
        assert_eq!(obs_ctx.seen.lock().unwrap().len(), 2);
    }

    #[derive(bevy_ecs::event::EntityEvent, Clone, Copy, Debug)]
    struct NonFromReady {
        #[event_target]
        entity: Entity,
        extra_rust_tag: u128,
    }

    #[test]
    fn non_from_ready_bridged_entity_event_with_supports_ready_trigger_and_propagation() {
        let mut world = World::new();
        let child_of_id = world.register_component::<ChildOf>();
        let mut reg = ModRegistry::new();
        reg.register_hierarchy(child_of_id, false).unwrap();
        let ready_handle = reg
            .register_bridged_entity_event_with::<NonFromReady>(
                &mut world,
                "bevy_scene::Ready",
                Some("Ready"),
                |w, entity| {
                    w.trigger(NonFromReady {
                        entity,
                        extra_rust_tag: 0xCAFE_BABE_1234_5678,
                    });
                },
            )
            .unwrap();

        let active = Arc::new(AtomicBool::new(true));
        let log = Box::new(LogCtx {
            events: Mutex::new(Vec::new()),
        });
        let log_ptr = (&*log as *const LogCtx).cast_mut().cast::<c_void>();

        let registered_observers = {
            let mut ctx = LoadContext::with_world_and_mod_id(
                reg.clone(),
                Arc::clone(&active),
                &mut world as *mut World,
                Some(ModId(7)),
            );
            let ready_desc = BamObserverDesc::new_lifecycle(
                BamEventKind::READY,
                core::ptr::null(),
                0,
                record_ready,
                log_ptr,
            )
            .with_name(c"on_non_from_ready".as_ptr());
            // SAFETY: Valid `LoadContext` and `BamObserverDesc`.
            let rr = unsafe { host_register_observer(ctx.as_ffi_ptr(), &ready_desc) };
            assert_eq!(rr, FfiResult::OK);
            reg = ctx.registry.clone();
            ctx.finish_init();
            core::mem::take(&mut ctx.registered_observers)
        };

        let _obs =
            spawn_registered_observers_in_world(&mut world, registered_observers, &reg, ModId(7));
        world.add_observer(|ev: bevy_ecs::observer::On<NonFromReady>| {
            assert_ne!(ev.extra_rust_tag, 0);
        });

        let parent = world.spawn_empty().id();
        let child = world.spawn(ChildOf(parent)).id();
        let parent_ffi = FfiEntity::from_bits(parent.to_bits());
        let child_ffi = FfiEntity::from_bits(child.to_bits());

        // 1. Direct host `world.trigger(NonFromReady { ... })` delivers synthesized 8-byte `FfiEntity`.
        world.trigger(NonFromReady {
            entity: child,
            extra_rust_tag: 99,
        });
        assert_eq!(*log.events.lock().unwrap(), vec![("ready", child_ffi)]);
        log.events.lock().unwrap().clear();

        // 2. `host_cmd_trigger` with `target == FfiEntity::NULL` + 8-byte `FfiEntity` wire payload
        //    must NOT read `size_of::<NonFromReady>()` (24 bytes) out of bounds.
        let snap = reg.snapshot_arc();
        let mut storage = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(7)), Some(Arc::clone(&active)))
                .with_registry(&reg);
            let wire_bits = child_ffi.to_bits();
            // SAFETY: `cmds` is valid and `wire_bits` is an initialized 8-byte `FfiEntity` payload.
            let rc = unsafe {
                crate::ffi::commands::host_cmd_trigger(
                    cmds.as_ffi_ptr(),
                    ready_handle,
                    FfiEntity::NULL,
                    (&raw const wire_bits).cast::<u8>(),
                    core::mem::size_of::<FfiEntity>(),
                )
            };
            assert_eq!(rc, FfiResult::OK);
        }
        storage.apply_to_world(&mut world, &snap);
        assert_eq!(*log.events.lock().unwrap(), vec![("ready", child_ffi)]);
        log.events.lock().unwrap().clear();

        // 3. `host_cmd_trigger_ex` with `BamEventKind::READY` + upward `ChildOf` propagation
        //    fires `NonFromReady` on `child` and then `parent`.
        let mut storage2 = HostCommandsStorage::new();
        {
            let mut cmds = HostCommands::new(&mut storage2, world.entity_allocator(), &snap)
                .with_mod_context(Some(ModId(7)), Some(active))
                .with_registry(&reg);
            let trig = BamTriggerDesc::new(BamComponent::INVALID, core::ptr::null_mut(), 0)
                .with_event_kind(BamEventKind::READY)
                .with_entity(child_ffi)
                .with_propagate(BamComponent::INVALID);
            // SAFETY: Valid `HostCommands` and `BamTriggerDesc`.
            let rt = unsafe { host_cmd_trigger_ex(cmds.as_ffi_ptr(), &trig) };
            assert_eq!(rt, FfiResult::OK);
        }
        storage2.apply_to_world(&mut world, &snap);
        assert_eq!(
            *log.events.lock().unwrap(),
            vec![("ready", child_ffi), ("ready", parent_ffi)]
        );
    }
}
