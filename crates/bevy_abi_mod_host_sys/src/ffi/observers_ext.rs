//! Dynamic observer execution (`ModObserverState`, `guest_observer_runner`), extended deferred
//! trigger (`cmd_trigger_ex`), and runtime observer (`cmd_observe`) entrypoints.

use core::ffi::c_void;
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
    message::Messages,
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
    handle_header::{
        AlignedBuf, decode_live_entity, decode_optional_entity, ffi_try, init_opt_out_ptr,
        validate_commands_mut,
    },
    loader::{LoadedModInfo, ModHost, ModId},
    registry::{EntityEventTriggerFn, ModRegistry, TriggerEventFn},
    sync_ptr::GuestFnPtr,
    system::{
        ModErrorPolicy, ModErrorPolicyConfig, ModSystemError, ModSystemFaulted,
        dispatch_mod_observer_fault_to_fallback_handler,
    },
};

/// ECS [`Component`] storing the shared [`GuestObserverBinding`] and per-observer enablement flag
/// on each spawned guest observer entity.
#[derive(Component)]
pub(crate) struct ModObserverState {
    pub(crate) binding: Arc<GuestObserverBinding>,
    pub(crate) enabled: Arc<AtomicBool>,
}

impl ModObserverState {
    pub(crate) fn new(binding: GuestObserverBinding) -> Self {
        Self {
            binding: Arc::new(binding),
            enabled: Arc::new(AtomicBool::new(true)),
        }
    }
}

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
    let Some((binding, enabled)) = world
        .get::<ModObserverState>(observer)
        .map(|state| (Arc::clone(&state.binding), Arc::clone(&state.enabled)))
    else {
        return;
    };
    if !binding.is_active.load(Ordering::Acquire) || !enabled.load(Ordering::Acquire) {
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

    if status == FfiResult::OK {
        if !storage.is_empty() {
            world.commands().queue(move |w: &mut World| {
                storage.apply_to_world(w, &snap);
            });
        }
        return;
    }

    storage.clear();
    let policy = world
        .get_resource::<ModErrorPolicyConfig>()
        .map_or(ModErrorPolicy::default(), |c| c.0);
    if !matches!(policy, ModErrorPolicy::DelegateToBevyFallback) {
        // Quarantine only this faulting observer; keep sibling systems/observers in the mod active.
        enabled.store(false, Ordering::Release);
    }

    let mod_id = binding.mod_id;
    let (mod_namespace, generation) = mod_id
        .and_then(|id| {
            world.get_resource::<ModHost>().and_then(|mh| {
                mh.get_mod(id)
                    .map(|m| (Some(m.name.to_string()), mh.mod_generation(&m.name)))
            })
        })
        .unwrap_or((None, 0));
    let panicked = status == FfiResult::PANIC_CAUGHT;
    let obs_name = binding.name.to_string();
    let last_run = world.change_tick();
    tracing::error!(
        observer = %obs_name,
        mod_id = ?mod_id,
        namespace = %mod_namespace.as_deref().unwrap_or("unscoped"),
        generation,
        %status,
        panicked,
        "guest mod observer faulted; discarding uncommitted commands and routing to FallbackErrorHandler"
    );

    let fault_msg = ModSystemFaulted {
        mod_id,
        mod_namespace: mod_namespace.clone(),
        generation,
        system_name: obs_name.clone(),
        status,
        panicked,
        last_error: None,
        init_error: None,
    };
    if let Some(id) = mod_id {
        let entity_opt = world
            .get_resource_mut::<ModHost>()
            .and_then(|mut host| host.increment_fault_count(id));
        if let Some(entity) = entity_opt
            && let Some(mut info) = world.get_mut::<LoadedModInfo>(entity)
        {
            info.fault_count = info.fault_count.saturating_add(1);
        }
    }
    if let Some(mut msgs) = world.get_resource_mut::<Messages<ModSystemFaulted>>() {
        msgs.write(fault_msg);
    }

    dispatch_mod_observer_fault_to_fallback_handler(
        &world,
        ModSystemError {
            mod_id,
            mod_namespace,
            generation,
            system_name: obs_name,
            status,
            panicked,
            last_error: None,
        },
        last_run,
    );
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
        let obs_entity = world.spawn((obs, ModObserverState::new(obs_binding))).id();
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

struct CustomTriggerDispatch<'a> {
    event_key: EventKey,
    entity_trigger_fn: Option<EntityEventTriggerFn>,
    trigger_fn: Option<TriggerEventFn>,
    watched_ids: &'a [ComponentId],
    payload: &'a mut AlignedBuf,
}

impl CustomTriggerDispatch<'_> {
    fn dispatch_at(&mut self, world: &mut World, target: Option<Entity>, propagate: bool) -> bool {
        let mut guard = DynamicTriggerGuard::enter(
            world,
            DynamicTriggerState {
                current_target: target,
                propagate,
            },
        );
        let w = guard.world_mut();
        if let Some(et_fn) = self.entity_trigger_fn {
            if let Some(t) = target
                && t != Entity::PLACEHOLDER
                && w.entities().contains(t)
            {
                et_fn(w, t);
            }
        } else if !self.watched_ids.is_empty()
            && let Some(t) = target
        {
            let mut trigger = EntityComponentsTrigger {
                components: self.watched_ids,
                ..Default::default()
            };
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityComponentsTrigger`.
            unsafe {
                w.trigger_dynamic_targets_components(
                    self.event_key,
                    t,
                    self.watched_ids,
                    event_mut,
                    trigger_mut,
                );
            }
        } else if let Some(tfn) = self.trigger_fn {
            // SAFETY: `payload.as_ptr()` holds `meta.size` bytes matching the host event type.
            unsafe {
                let _ = tfn(w, target, self.payload.as_ptr());
            }
        } else if let Some(t) = target {
            let mut trigger = EntityTrigger;
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `EntityTrigger`.
            unsafe {
                w.trigger_dynamic_targets(self.event_key, t, event_mut, trigger_mut);
            }
        } else {
            let mut trigger = GlobalTrigger;
            let event_mut = self.payload.as_ptr_mut();
            let trigger_mut = PtrMut::from(&mut trigger);
            // SAFETY: `event_key` matches `payload` layout and `trigger_mut` is `GlobalTrigger`.
            unsafe {
                w.trigger_dynamic(self.event_key, event_mut, trigger_mut);
            }
        }
        guard.propagate()
    }
}

/// Triggers a single Bevy lifecycle event struct (`AddEvent`, `InsertEvent`, etc.) for `target`.
///
/// # Safety
/// `event_key` must match the Bevy lifecycle event type `E`.
unsafe fn trigger_lifecycle<E>(
    world: &mut World,
    event_key: EventKey,
    target: Entity,
    watched_ids: &[ComponentId],
    mut event: E,
) {
    let mut trigger = EntityComponentsTrigger {
        components: watched_ids,
        ..Default::default()
    };
    // SAFETY: Caller guarantees `event_key` matches `E` and `EntityComponentsTrigger`.
    unsafe {
        world.trigger_dynamic_targets_components(
            event_key,
            target,
            watched_ids,
            PtrMut::from(&mut event),
            PtrMut::from(&mut trigger),
        );
    }
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
    match kind {
        BamEventKind::ADD => {
            // SAFETY: `ADD` expects `AddEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::ADD,
                    target,
                    watched_ids,
                    AddEvent { entity: target },
                );
            }
        }
        BamEventKind::INSERT => {
            // SAFETY: `INSERT` expects `InsertEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::INSERT,
                    target,
                    watched_ids,
                    InsertEvent { entity: target },
                );
            }
        }
        BamEventKind::DISCARD => {
            // SAFETY: `DISCARD` expects `DiscardEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::DISCARD,
                    target,
                    watched_ids,
                    DiscardEvent { entity: target },
                );
            }
        }
        BamEventKind::REMOVE => {
            // SAFETY: `REMOVE` expects `RemoveEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::REMOVE,
                    target,
                    watched_ids,
                    RemoveEvent { entity: target },
                );
            }
        }
        BamEventKind::DESPAWN => {
            // SAFETY: `DESPAWN` expects `DespawnEvent`.
            unsafe {
                trigger_lifecycle(
                    w,
                    bevy_ecs::lifecycle::DESPAWN,
                    target,
                    watched_ids,
                    DespawnEvent { entity: target },
                );
            }
        }
        _ => {}
    }
    guard.propagate()
}

/// Dispatches a trigger at `target` and, when `is_propagating` is true, bubbles up `prop_rel`
/// parents with cycle detection until stopped or no live parent remains.
fn propagate_trigger(
    world: &mut World,
    target: Option<Entity>,
    is_propagating: bool,
    prop_rel: PropagateRelationship,
    mut dispatch: impl FnMut(&mut World, Option<Entity>, bool) -> bool,
) {
    if let Some(t) = target
        && !world.entities().contains(t)
    {
        return;
    }
    if !is_propagating {
        let _ = dispatch(world, target, false);
        return;
    }
    let Some(mut current) = target else {
        return;
    };
    let mut visited = HashSet::new();
    visited.insert(current);
    loop {
        if !dispatch(world, Some(current), true) {
            break;
        }
        let Some(next) = next_propagation_parent(world, current, prop_rel) else {
            break;
        };
        if next == Entity::PLACEHOLDER || !world.entities().contains(next) || !visited.insert(next)
        {
            break;
        }
        current = next;
    }
}

/// Reads an unaligned 8-byte `FfiEntity` from `data_ptr` and decodes it as a live [`Entity`].
///
/// # Safety
/// `data_ptr` must be non-null and readable for 8 bytes.
#[inline]
unsafe fn decode_entity_from_bytes(data_ptr: *const u8) -> Result<Entity, FfiResult> {
    // SAFETY: Caller guarantees `data_ptr` is readable for 8 bytes.
    let bits = unsafe { core::ptr::read_unaligned(data_ptr.cast::<u64>()) };
    decode_live_entity(FfiEntity::from_bits(bits))
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
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamTriggerDesc::MIN_V0_2_SIZE`.
    let parsed = ffi_try!(unsafe { read_descriptor::<BamTriggerDesc>(desc.cast::<c_void>()) });
    let val = parsed.validate();
    if val.is_err() {
        return val;
    }

    let snap = host_cmds.snapshot();
    let mut target_entity = ffi_try!(decode_optional_entity(parsed.entity()));

    let is_propagating = parsed.is_propagating();
    let prop_rel = if is_propagating && parsed.propagate_relationship.is_valid() {
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
        let Some(event_key) = meta.event_key else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource()
            || meta.is_message()
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
                // SAFETY: `parsed.data_ptr` is non-null and readable for 8 bytes.
                target_entity = Some(ffi_try!(unsafe {
                    decode_entity_from_bytes(parsed.data_ptr)
                }));
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
            ffi_try!(unsafe { AlignedBuf::copy_from(parsed.data_ptr, meta.size, meta.align) });
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;

        host_cmds.queue_custom(move |world, _snap| {
            let mut ctx = CustomTriggerDispatch {
                event_key,
                entity_trigger_fn,
                trigger_fn,
                watched_ids: &watched_ids,
                payload: &mut payload,
            };
            propagate_trigger(world, target_entity, is_propagating, prop_rel, |w, t, p| {
                ctx.dispatch_at(w, t, p)
            });
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
            propagate_trigger(
                world,
                Some(start_target),
                is_propagating,
                prop_rel,
                |w, t, p| {
                    let Some(ent) = t else {
                        return false;
                    };
                    dispatch_lifecycle_at_target(w, kind, ent, &watched_ids, p)
                },
            );
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
        let Some(event_key) = meta.event_key else {
            return FfiResult::INVALID_COMPONENT;
        };
        if meta.is_resource() || meta.is_message() {
            return FfiResult::INVALID_COMPONENT;
        }
        if target_entity.is_none() && parsed.data_len == core::mem::size_of::<FfiEntity>() {
            // SAFETY: `parsed.data_len == 8` and `parsed.validate()` verified non-null `data_ptr`.
            target_entity = Some(ffi_try!(unsafe {
                decode_entity_from_bytes(parsed.data_ptr)
            }));
        } else if parsed.data_len != 0 && parsed.data_len != meta.size {
            return FfiResult::INVALID_ARGUMENT;
        }
        let Some(start_target) = target_entity else {
            return FfiResult::INVALID_ENTITY;
        };
        let entity_trigger_fn = meta.entity_event_trigger_fn;
        let trigger_fn = meta.trigger_event_fn;
        let ffi_bytes = FfiEntity::from_bits(start_target.to_bits()).to_bits();
        // SAFETY: `ffi_bytes` is a local 8-byte `u64` matching `FfiEntity` layout.
        let mut payload = ffi_try!(unsafe {
            AlignedBuf::copy_from(
                (&raw const ffi_bytes).cast::<u8>(),
                core::mem::size_of::<FfiEntity>(),
                core::mem::align_of::<FfiEntity>(),
            )
        });

        host_cmds.queue_custom(move |world, _snap| {
            let mut ctx = CustomTriggerDispatch {
                event_key,
                entity_trigger_fn,
                trigger_fn,
                watched_ids: &watched_ids,
                payload: &mut payload,
            };
            propagate_trigger(
                world,
                Some(start_target),
                is_propagating,
                prop_rel,
                |w, t, p| ctx.dispatch_at(w, t, p),
            );
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
    // SAFETY: Caller guarantees writable `FfiEntity` storage if non-null.
    let out_slot = ffi_try!(unsafe { init_opt_out_ptr(out_entity, FfiEntity::NULL) });
    // SAFETY: Validates `commands` against `COMMANDS_MAGIC` under `STRICT_FFI_CHECKS`.
    let host_cmds = ffi_try!(unsafe { validate_commands_mut(commands) });
    // SAFETY: `read_descriptor` checks null and `struct_size >= BamObserverDesc::MIN_V0_2_SIZE`.
    let parsed = ffi_try!(unsafe { read_descriptor::<BamObserverDesc>(desc.cast::<c_void>()) });
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
    if let Some(slot) = out_slot {
        *slot = FfiEntity::from_bits(obs_entity.to_bits());
    }

    host_cmds.queue_custom(move |world, _snap| {
        if !binding.is_active.load(Ordering::Acquire) {
            return;
        }
        let obs = build_observer_component(&binding);
        if let Ok(mut e) = world.spawn_empty_at(obs_entity) {
            e.insert((obs, ModObserverState::new(binding)));
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
#[path = "../../tests/unit/observers_ext_tests.rs"]
mod tests;
